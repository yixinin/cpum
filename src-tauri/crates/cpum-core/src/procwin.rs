//! Windows process low-level operations: affinity / CPU Sets / three
//! priority classes / lightweight enumeration.
//!
//! This module is the only "write entry point" into Win32 in cpum. It is
//! shared by the Tauri GUI and the Windows service to keep their behavior
//! consistent. GUI-only metrics collection still lives in `process.rs` in
//! the main crate.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::mem::size_of;

use windows::core::{w, PCSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, HANDLE, LUID, WIN32_ERROR, ERROR_NOT_ALL_ASSIGNED,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY, SE_PRIVILEGE_ENABLED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, THREADENTRY32, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::SystemInformation::{
    GetSystemCpuSetInformation, CpuSetInformation, GROUP_AFFINITY, SYSTEM_CPU_SET_INFORMATION,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetPriorityClass, GetProcessAffinityMask, GetProcessDefaultCpuSets,
    GetProcessInformation, OpenProcessToken,
    GetThreadGroupAffinity, OpenProcess, OpenThread, QueryFullProcessImageNameW, SetPriorityClass,
    SetProcessAffinityMask, SetProcessDefaultCpuSets, SetProcessInformation,
    SetThreadGroupAffinity, GetActiveProcessorCount, GetActiveProcessorGroupCount,
    MEMORY_PRIORITY, MEMORY_PRIORITY_INFORMATION,
    PROCESS_CREATION_FLAGS, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_LIMITED_INFORMATION,
    THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
};

// =========================================================================
// SeDebugPrivilege
// =========================================================================

/// Enable `SeDebugPrivilege` on the current process.
///
/// Required to reliably open processes owned by another account (or running at
/// a higher integrity level). `LocalSystem` holds the privilege but may start
/// with it disabled; an elevated administrator token holds it as well. A
/// standard / filtered UAC token does not, and this call fails for it - which
/// is exactly why the GUI delegates to the service instead.
pub fn enable_debug_privilege() -> Result<(), String> {
    unsafe {
        let mut token = Default::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

        let mut luid = LUID::default();
        if let Err(error) = LookupPrivilegeValueW(None, w!("SeDebugPrivilege"), &mut luid) {
            let _ = CloseHandle(token);
            return Err(format!("LookupPrivilegeValueW(SeDebugPrivilege): {error}"));
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        SetLastError(WIN32_ERROR(0));
        let adjust_result =
            AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
        let last_error = GetLastError();
        let _ = CloseHandle(token);

        adjust_result.map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
        if last_error == ERROR_NOT_ALL_ASSIGNED {
            return Err("current account does not hold SeDebugPrivilege".to_string());
        }
    }
    Ok(())
}

// =========================================================================
// mask utilities
// =========================================================================

/// Parse a hex mask string ("0xFF" / "FF" / "0xff"; leading/trailing
/// whitespace is tolerated).
pub fn parse_hex_mask(s: &str) -> Result<u64, String> {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.is_empty() {
        return Err(format!("mask cannot be empty (raw input: \"{}\")", s));
    }
    if trimmed.len() > 16 {
        return Err(format!(
            "mask exceeds 64 bits ({} hex digits): \"{}\"",
            trimmed.len(),
            s
        ));
    }
    u64::from_str_radix(trimmed, 16).map_err(|e| format!("failed to parse mask \"{}\": {}", s, e))
}

/// Format a u64 mask as a prefixed hex string (e.g. "0xFFFFFFFF").
pub fn mask_to_hex(mask: u64) -> String {
    format!("0x{:X}", mask)
}

// =========================================================================
// Multi-processor groups (>64 logical processors: Threadripper /
// dual-socket workstations)
// =========================================================================

/// A set of affinity masks, one per processor group (index = group number).
///
/// On a single-group system this is equivalent to a bare `u64` (only
/// `masks[0]`). On multi-group systems, each element is the per-group
/// KAFFINITY. Global LP numbering = `group * 64 + bit_index_within_group`,
/// consistent with the global LP indices produced by the topology module.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupMasks(pub Vec<u64>);

impl GroupMasks {
    /// Single-group constructor (compatible with the existing u64 mask
    /// path).
    pub fn single(mask: u64) -> Self {
        Self(vec![mask])
    }

    /// All group masks are zero -> no LPs selected.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&m| m == 0)
    }

    /// Total number of bits set across all groups.
    pub fn total_bits(&self) -> u32 {
        self.0.iter().map(|m| m.count_ones()).sum()
    }

    /// Parse from a list of hex strings (list index = group number).
    pub fn from_hex_list(list: &[String]) -> Result<Self, String> {
        if list.is_empty() {
            return Err("group mask list cannot be empty".to_string());
        }
        let mut masks = Vec::with_capacity(list.len());
        for (g, s) in list.iter().enumerate() {
            let m = parse_hex_mask(s).map_err(|e| format!("invalid mask for group {g}: {e}"))?;
            masks.push(m);
        }
        Ok(Self(masks))
    }

    /// Serialize to a list of hex strings (inverse of `from_hex_list`).
    pub fn to_hex_list(&self) -> Vec<String> {
        self.0.iter().map(|&m| mask_to_hex(m)).collect()
    }
}

/// Number of active processor groups (regular desktops/laptops are always
/// 1; systems with >64 LPs have >= 2).
pub fn active_group_count() -> u16 {
    unsafe { GetActiveProcessorGroupCount() }
}

/// Number of active logical processors in the specified group.
pub fn active_processor_count(group: u16) -> u32 {
    unsafe { GetActiveProcessorCount(group) }
}

/// Validate masks against a supplied processor-group layout. Kept separate
/// from the Win32 query so migration and boundary cases can be unit tested on
/// ordinary single-group hardware.
pub fn validate_group_masks_for_counts(masks: &[u64], active_counts: &[u32]) -> Result<(), String> {
    if masks.is_empty() {
        return Err("group mask list cannot be empty".to_string());
    }
    if masks.len() > active_counts.len() {
        return Err(format!(
            "mask group count {} exceeds the system processor group count {}",
            masks.len(), active_counts.len()
        ));
    }
    for (group, &mask) in masks.iter().enumerate() {
        let count = active_counts[group];
        if count == 0 {
            if mask != 0 {
                return Err(format!("group {group} has no active logical processors"));
            }
            continue;
        }
        if count < 64 && (mask >> count) != 0 {
            return Err(format!(
                "mask for group {group} selects logical processors outside its active range (0..{})",
                count - 1
            ));
        }
    }
    if masks.iter().all(|&mask| mask == 0) {
        return Err("mask is all zero: select at least one logical processor".to_string());
    }
    Ok(())
}

/// Validate masks against the currently active Windows processor groups.
pub fn validate_group_masks(masks: &[u64]) -> Result<(), String> {
    let counts: Vec<u32> = (0..active_group_count()).map(active_processor_count).collect();
    validate_group_masks_for_counts(masks, &counts)
}

/// Enumerate all thread IDs of a process (filter from a `TH32CS_SNAPTHREAD`
/// snapshot). Needed by the cross-group hard-affinity write path:
/// `SetProcessAffinityMask` only affects the primary group, so any bits
/// outside that group must be applied per-thread via
/// `SetThreadGroupAffinity`.
fn enumerate_process_threads(pid: u32) -> Result<Vec<u32>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot(THREAD) failed: {e}"))?;
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut tids = Vec::new();
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    tids.push(entry.th32ThreadID);
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(tids)
    }
}

/// Strict affinity (multi-group): `SetThreadGroupAffinity` per thread.
///
/// Each thread keeps its current group when the group has available bits
/// in the target mask, otherwise it migrates to the first group with
/// available bits - matching Task Manager's affinity semantics on >64 LP
/// systems. With a single mask (length <= 1) this automatically falls back
/// to the `SetProcessAffinityMask` fast path.
pub fn set_process_affinity_group_masks(pid: u32, masks: &[u64]) -> Result<(), String> {
    validate_group_masks(masks)?;
    if masks.len() <= 1 {
        let m = masks.first().copied().unwrap_or(0);
        return set_process_affinity(pid, m);
    }
    let group_count = active_group_count() as usize;
    // Normalize to the system group count (missing groups are 0 -> no LPs
    // selected in that group).
    let mut full = vec![0u64; group_count];
    for (g, &m) in masks.iter().enumerate() {
        full[g] = m;
    }
    if full.iter().all(|&m| m == 0) {
        return Err(format!("mask is all zero (PID {pid}): select at least one logical processor"));
    }

    let tids = enumerate_process_threads(pid)?;
    if tids.is_empty() {
        return Err(format!("process {pid} has no enumerable threads"));
    }

    let first_nonzero_group = full.iter().position(|&m| m != 0).map(|i| i as u16);
    let mut failed: u32 = 0;
    let total_threads = tids.len() as u32;
    for tid in tids {
        unsafe {
            let Ok(h) = OpenThread(THREAD_SET_INFORMATION | THREAD_QUERY_INFORMATION, false, tid)
            else {
                failed += 1;
                continue;
            };
            let mut applied = false;
            // Thread stays in its current group (when the group has
            // available bits), otherwise migrates to the first non-zero
            // group.
            let mut cur = GROUP_AFFINITY::default();
            let target_group = if GetThreadGroupAffinity(h, &mut cur).as_bool()
                && (cur.Group as usize) < full.len()
                && full[cur.Group as usize] != 0
            {
                Some(cur.Group)
            } else {
                first_nonzero_group
            };
            if let Some(g) = target_group {
                let ga = GROUP_AFFINITY {
                    Mask: full[g as usize] as usize,
                    Group: g,
                    Reserved: [0; 3],
                };
                applied = SetThreadGroupAffinity(h, &ga, None).as_bool();
            }
            if !applied {
                failed += 1;
            }
            let _ = CloseHandle(h);
        }
    }
    // Partial failures (e.g. a thread racing the snapshot) are not treated
    // as a whole failure - the threads that did get set stay set.
    if failed == total_threads {
        return Err(format!("SetThreadGroupAffinity failed for all threads (PID {pid})"));
    }
    Ok(())
}

/// Aggregate per-thread group affinity of **every** process in one pass
/// and produce a per-process, per-group mask.
///
/// This is the only reliable way to read true affinity on multi-group
/// systems (`GetProcessAffinityMask` only returns the primary group): one
/// thread snapshot + per-thread `GetThreadGroupAffinity` + per-group union.
/// Call it once during the enumeration phase; do not put it in the
/// per-second metrics stream.
pub fn aggregate_group_affinity_by_pid() -> Result<HashMap<u32, Vec<u64>>, String> {
    let group_count = active_group_count() as usize;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot(THREAD) failed: {e}"))?;
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut result: HashMap<u32, Vec<u64>> = HashMap::new();
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                let pid = entry.th32OwnerProcessID;
                let tid = entry.th32ThreadID;
                if let Ok(h) = OpenThread(THREAD_QUERY_INFORMATION, false, tid) {
                    let mut ga = GROUP_AFFINITY::default();
                    if GetThreadGroupAffinity(h, &mut ga).as_bool() {
                        let masks = result
                            .entry(pid)
                            .or_insert_with(|| vec![0u64; group_count]);
                        let g = ga.Group as usize;
                        if g < group_count {
                            masks[g] |= ga.Mask as u64;
                        }
                    }
                    let _ = CloseHandle(h);
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(result)
    }
}

/// Read per-group affinity masks of a single process (None when no thread
/// can be opened).
pub fn get_process_group_masks(pid: u32) -> Option<Vec<u64>> {
    let group_count = active_group_count() as usize;
    let tids = enumerate_process_threads(pid).ok()?;
    let mut masks = vec![0u64; group_count];
    let mut any = false;
    for tid in tids {
        unsafe {
            let Ok(h) = OpenThread(THREAD_QUERY_INFORMATION, false, tid) else {
                continue;
            };
            let mut ga = GROUP_AFFINITY::default();
            if GetThreadGroupAffinity(h, &mut ga).as_bool() {
                let g = ga.Group as usize;
                if g < group_count {
                    masks[g] |= ga.Mask as u64;
                    any = true;
                }
            }
            let _ = CloseHandle(h);
        }
    }
    if any { Some(masks) } else { None }
}

// =========================================================================
// ntdll dynamic resolution (NtQuery/NtSetInformationProcess)
// =========================================================================
// Both ProcessIoPriority=33 and ProcessNetworkIoCounters=114 are
// undocumented info classes, not exposed by the windows crate; the
// function pointer is fetched via GetProcAddress. The GUI metrics stream
// also reuses the NtQuery resolution done here.

#[allow(non_snake_case)]
pub type NtQueryInformationProcessFn = unsafe extern "system" fn(
    ProcessHandle: HANDLE,
    ProcessInformationClass: u32,
    ProcessInformation: *mut std::ffi::c_void,
    ProcessInformationLength: u32,
    ReturnLength: *mut u32,
) -> i32; // NTSTATUS

#[allow(non_snake_case)]
pub type NtSetInformationProcessFn = unsafe extern "system" fn(
    ProcessHandle: HANDLE,
    ProcessInformationClass: u32,
    ProcessInformation: *mut std::ffi::c_void,
    ProcessInformationLength: u32,
) -> i32; // NTSTATUS

fn resolve_ntdll_fn<T>(name: &[u8]) -> Option<T> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let addr = GetProcAddress(ntdll, PCSTR(name.as_ptr()))?;
        // FARPROC is a fn pointer (8 bytes, same as the target T);
        // transmute_copy avoids the transmute limitation for generic T of
        // unknown size.
        Some(std::mem::transmute_copy(&addr))
    }
}

static NT_QUERY_PROCESS: Lazy<Option<NtQueryInformationProcessFn>> =
    Lazy::new(|| resolve_ntdll_fn(b"NtQueryInformationProcess\0"));

static NT_SET_PROCESS: Lazy<Option<NtSetInformationProcessFn>> =
    Lazy::new(|| resolve_ntdll_fn(b"NtSetInformationProcess\0"));

/// ntdll!NtQueryInformationProcess (None on resolution failure, e.g. when
/// a security product has hooked the export).
pub fn nt_query_information_process() -> Option<NtQueryInformationProcessFn> {
    *NT_QUERY_PROCESS
}

/// ntdll!NtSetInformationProcess
pub fn nt_set_information_process() -> Option<NtSetInformationProcessFn> {
    *NT_SET_PROCESS
}

/// IO priority info class (undocumented).
const PROCESS_IO_PRIORITY_CLASS: u32 = 33;

// =========================================================================
// Handle helpers
// =========================================================================

/// Open a process with the rights needed for modification (covers
/// affinity + CPU Sets + priority writes).
fn open_for_modify(pid: u32) -> Result<HANDLE, String> {
    unsafe {
        OpenProcess(
            PROCESS_SET_INFORMATION | PROCESS_SET_LIMITED_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
        .map_err(|e| format!("OpenProcess failed (PID {}): {}", pid, e))
    }
}

fn open_query_limited(pid: u32) -> Result<HANDLE, String> {
    unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess(QUERY_LIMITED) failed (PID {}): {}", pid, e))
    }
}

fn close(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

// =========================================================================
// Three priority classes (CPU / IO / memory)
// =========================================================================

/// Snapshot of the three priority classes (None = read failed / not
/// supported / insufficient privilege).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessPriorities {
    /// Raw CPU priority class value (0x40=Idle, 0x4000=BelowNormal,
    /// 0x20=Normal, 0x8000=AboveNormal, 0x80=High, 0x100=Realtime).
    pub priority_class: Option<u32>,
    /// IO priority (0=Very Low, 1=Low, 2=Normal, 3=High [reserved for
    /// system]).
    pub io_priority: Option<u32>,
    /// Memory priority (1=Very Low, 2=Low, 3=Medium, 4=Below Normal,
    /// 5=Normal).
    pub memory_priority: Option<u32>,
}

/// Scheduling-order rank of a CPU priority class (higher = higher priority;
/// 0=Low ... 5=Realtime).
///
/// The priority class constants are **bit flags**, not scheduling-order
/// values - the numeric ordering does not match the priority order
/// (NORMAL=0x20 < BELOW_NORMAL=0x4000, but NORMAL has the higher
/// scheduling rank). Any "is priority higher than X" comparison must go
/// through this function and must not compare the raw values directly.
/// Unknown values (including combination values such as EcoQoS background
/// mode flags) return None; callers should handle them conservatively.
pub fn priority_class_rank(class: u32) -> Option<u8> {
    match class {
        0x40 => Some(0),   // IDLE_PRIORITY_CLASS
        0x4000 => Some(1), // BELOW_NORMAL_PRIORITY_CLASS
        0x20 => Some(2),   // NORMAL_PRIORITY_CLASS
        0x8000 => Some(3), // ABOVE_NORMAL_PRIORITY_CLASS
        0x80 => Some(4),   // HIGH_PRIORITY_CLASS
        0x100 => Some(5),  // REALTIME_PRIORITY_CLASS
        _ => None,
    }
}

/// Read the three priority classes of a process (handle must already be
/// open; a single field failure leaves only that field as None).
pub fn read_priorities_with_handle(handle: HANDLE) -> ProcessPriorities {
    unsafe {
        // 1. CPU priority class (GetPriorityClass returns 0 on failure).
        let priority_class = {
            let pc = GetPriorityClass(handle);
            if pc != 0 { Some(pc) } else { None }
        };

        // 2. IO priority (NtQueryInformationProcess(ProcessIoPriority=33)).
        let io_priority = match nt_query_information_process() {
            None => None,
            Some(nt_fn) => {
                let mut io: u32 = 0;
                let mut returned: u32 = 0;
                let status = nt_fn(
                    handle,
                    PROCESS_IO_PRIORITY_CLASS,
                    &mut io as *mut u32 as *mut std::ffi::c_void,
                    size_of::<u32>() as u32,
                    &mut returned,
                );
                if status == 0 { Some(io) } else { None }
            }
        };

        // 3. Memory priority (GetProcessInformation(ProcessMemoryPriority),
        //    Win8+).
        let memory_priority = {
            let mut info: MEMORY_PRIORITY_INFORMATION = std::mem::zeroed();
            if GetProcessInformation(
                handle,
                windows::Win32::System::Threading::ProcessMemoryPriority,
                &mut info as *mut _ as *mut std::ffi::c_void,
                size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            )
            .is_ok()
            {
                Some(info.MemoryPriority.0)
            } else {
                None
            }
        };

        ProcessPriorities { priority_class, io_priority, memory_priority }
    }
}

/// Read the three priority classes of the given PID (returns all-None when
/// the handle cannot be opened).
pub fn get_process_priorities(pid: u32) -> ProcessPriorities {
    unsafe {
        // Prefer full query rights (NtQuery needs them for IO priority);
        // fall back to LIMITED when that's denied.
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid)
            .or_else(|_| OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid));
        match handle {
            Ok(h) => {
                let p = read_priorities_with_handle(h);
                close(h);
                p
            }
            Err(_) => ProcessPriorities::default(),
        }
    }
}

/// Set the CPU priority class (Win32 raw value; validity is the caller's
/// responsibility / enforced by `rule::validate_rule`).
pub fn set_process_priority_class(pid: u32, class: u32) -> Result<(), String> {
    unsafe {
        let handle = open_for_modify(pid)?;
        let result = SetPriorityClass(handle, PROCESS_CREATION_FLAGS(class));
        close(handle);
        result.map_err(|e| format!("SetPriorityClass failed (PID {}, class=0x{:X}): {}", pid, class, e))
    }
}

/// Set the IO priority (0=Very Low, 1=Low, 2=Normal; 3=High is reserved
/// for system).
pub fn set_process_io_priority(pid: u32, io_priority: u32) -> Result<(), String> {
    let nt_fn = nt_set_information_process()
        .ok_or_else(|| "cannot resolve ntdll!NtSetInformationProcess".to_string())?;
    unsafe {
        let handle = open_for_modify(pid)?;
        let mut value = io_priority;
        let status = nt_fn(
            handle,
            PROCESS_IO_PRIORITY_CLASS,
            &mut value as *mut u32 as *mut std::ffi::c_void,
            size_of::<u32>() as u32,
        );
        close(handle);
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "NtSetInformationProcess(ProcessIoPriority) failed (PID {}, value={}): NTSTATUS=0x{:08X}",
                pid, io_priority, status as u32
            ))
        }
    }
}

/// Set the memory priority (1=Very Low, 2=Low, 3=Medium, 4=Below Normal,
/// 5=Normal).
pub fn set_process_memory_priority(pid: u32, memory_priority: u32) -> Result<(), String> {
    unsafe {
        let handle = open_for_modify(pid)?;
        let info = MEMORY_PRIORITY_INFORMATION {
            MemoryPriority: MEMORY_PRIORITY(memory_priority),
        };
        let result = SetProcessInformation(
            handle,
            windows::Win32::System::Threading::ProcessMemoryPriority,
            &info as *const _ as *const std::ffi::c_void,
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        );
        close(handle);
        result.map_err(|e| {
            format!(
                "SetProcessInformation(ProcessMemoryPriority) failed (PID {}, value={}): {}",
                pid, memory_priority, e
            )
        })
    }
}

// =========================================================================
// Affinity (strict mode: hard mask)
// =========================================================================

/// Read `(process affinity mask, system affinity mask)` of the specified
/// process; either being None means it could not be read.
pub fn get_process_affinity(pid: u32) -> Result<(Option<u64>, Option<u64>), String> {
    let handle = open_query_limited(pid)?;
    let masks = read_affinity_with_handle(handle);
    close(handle);
    Ok(masks)
}

/// Read `(process mask, system mask)` using an already-open handle.
pub fn read_affinity_with_handle(handle: HANDLE) -> (Option<u64>, Option<u64>) {
    unsafe {
        let mut process_mask: usize = 0;
        let mut system_mask: usize = 0;
        match GetProcessAffinityMask(handle, &mut process_mask, &mut system_mask) {
            Ok(_) => (Some(process_mask as u64), Some(system_mask as u64)),
            Err(_) => (None, None),
        }
    }
}

/// Strict affinity: hard mask (`SetProcessAffinityMask`); the process is
/// pinned to the selected cores.
pub fn set_process_affinity(pid: u32, mask: u64) -> Result<(), String> {
    unsafe {
        let handle = open_for_modify(pid)?;
        let result = SetProcessAffinityMask(handle, mask as usize);
        close(handle);
        result.map_err(|e| {
            format!("SetProcessAffinityMask failed (PID {}, mask=0x{:X}): {}", pid, mask, e)
        })
    }
}

// =========================================================================
// CPU Sets (soft mode, Win10 1803+)
// =========================================================================
// CPU Sets are addressed by a "CPU Set ID" (u32) rather than an LP index.
// LP index is group-local, so the mapping key is always `(group, index)`;
// using an index-only map would silently select CPUs in every group with the
// same bit number. The IDs are stable for the boot session.

struct CpuSetEntry {
    group: u16,
    lp_index: u8,
    id: u32,
}

static CPU_SETS: Lazy<Vec<CpuSetEntry>> =
    Lazy::new(|| query_system_cpu_sets().unwrap_or_default());

/// Whether the system supports CPU Sets (when unsupported, soft rules
/// automatically fall back to the hard mask).
pub fn cpu_sets_available() -> bool {
    !CPU_SETS.is_empty()
}

/// Enumerate the system CPU sets and build a `(group, LP index)` -> CPU Set
/// ID map.
fn query_system_cpu_sets() -> Result<Vec<CpuSetEntry>, String> {
    unsafe {
        let mut needed: u32 = 0;
        // First call only fetches the required byte count (expected to
        // return ERROR_INSUFFICIENT_BUFFER).
        let _ = GetSystemCpuSetInformation(None, 0, &mut needed, None, None);
        if needed == 0 {
            return Err("GetSystemCpuSetInformation returned no data (system may not support CPU Sets)".into());
        }
        let entry_size = size_of::<SYSTEM_CPU_SET_INFORMATION>();
        let count = (needed as usize / entry_size).max(1);
        let mut buffer = vec![0u8; count * entry_size];
        let mut returned: u32 = 0;
        let ptr = buffer.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION;
        GetSystemCpuSetInformation(
            Some(ptr),
            buffer.len() as u32,
            &mut returned,
            None,
            None,
        )
        .ok()
        .map_err(|e| format!("GetSystemCpuSetInformation failed: {e}"))?;

        let valid = (returned as usize / entry_size).min(count);
        let mut entries = Vec::with_capacity(valid);
        for i in 0..valid {
            let info = &*ptr.add(i);
            // Official docs require skipping non-CpuSetInformation entries
            // (current systems only return that type; the check is a
            // future-proofing guard so we don't interpret unrelated data
            // as CPU Sets when new entry types are added).
            if info.Type != CpuSetInformation {
                continue;
            }
            let cpu_set = &info.Anonymous.CpuSet;
            entries.push(CpuSetEntry {
                group: cpu_set.Group,
                lp_index: cpu_set.LogicalProcessorIndex,
                id: cpu_set.Id,
            });
        }
        Ok(entries)
    }
}

/// Convert an LP bitmask to the corresponding list of CPU Set IDs.
pub fn cpu_set_ids_for_mask(mask: u64) -> Result<Vec<u32>, String> {
    cpu_set_ids_for_group_masks(&[mask])
}

/// Convert per-processor-group masks to CPU Set IDs. CPU Set IDs are global
/// for the boot session, while LP bit positions are only meaningful inside a
/// processor group.
pub fn cpu_set_ids_for_group_masks(masks: &[u64]) -> Result<Vec<u32>, String> {
    if CPU_SETS.is_empty() {
        return Err("system does not support CPU Sets (requires Windows 10 1803+)".into());
    }
    let ids = cpu_set_ids_for_group_masks_from_entries(masks, &CPU_SETS);
    if ids.is_empty() {
        return Err("selected masks contain no CPU Sets".into());
    }
    Ok(ids)
}

/// Pure mapper used by the Win32 path and tests. Each entry's group must be
/// checked before its group-local bit index; this is the cross-group safety
/// invariant for soft scheduling.
fn cpu_set_ids_for_group_masks_from_entries(masks: &[u64], entries: &[CpuSetEntry]) -> Vec<u32> {
    entries
        .iter()
        .filter(|e| masks.get(e.group as usize).is_some_and(|mask| mask & (1u64 << e.lp_index) != 0))
        .map(|e| e.id)
        .collect()
}

/// Soft affinity: set the process's default CPU Sets and restore the hard
/// mask to the system mask (releasing any leftover strict pinning so the
/// scheduler can drift under load peaks).
pub fn set_process_soft_affinity(pid: u32, mask: u64) -> Result<(), String> {
    set_process_soft_affinity_group_masks(pid, &[mask])
}

pub fn set_process_soft_affinity_group_masks(pid: u32, masks: &[u64]) -> Result<(), String> {
    validate_group_masks(masks)?;
    let ids = cpu_set_ids_for_group_masks(masks)?;
    unsafe {
        let handle = open_for_modify(pid)?;
        // 1. Default CPU Sets (soft constraint).
        let set_result = SetProcessDefaultCpuSets(handle, Some(&ids)).ok();
        // 2. Restore the hard mask to the system mask (best-effort;
        //    failure does not affect the already-applied CPU Sets).
        let restore_result = if set_result.is_ok() {
            let (_, system_mask) = read_affinity_with_handle(handle);
            system_mask.map(|sm| SetProcessAffinityMask(handle, sm as usize))
        } else {
            None
        };
        close(handle);

        set_result.map_err(|e| format!("SetProcessDefaultCpuSets failed (PID {}): {}", pid, e))?;
        if let Some(Err(e)) = restore_result {
            return Err(format!(
                "SetProcessAffinityMask(restore system mask) failed (PID {}): {}",
                pid, e
            ));
        }
        Ok(())
    }
}

/// Read the `(group, LP index)` locations of a process's default CPU Sets
/// (empty = no soft constraint set). Returning the group is essential on
/// >64-LP systems because the LP index alone is ambiguous.
pub fn get_process_default_cpu_set_locations(pid: u32) -> Result<Vec<(u16, u8)>, String> {
    unsafe {
        let handle = open_query_limited(pid)?;
        let mut buffer = vec![0u32; 128];
        let mut returned: u32 = 0;
        let result = GetProcessDefaultCpuSets(handle, Some(&mut buffer), &mut returned).ok();
        close(handle);
        result.map_err(|e| format!("GetProcessDefaultCpuSets failed (PID {}): {}", pid, e))?;

        let count = (returned as usize).min(buffer.len());
        let locations: Vec<(u16, u8)> = buffer[..count]
            .iter()
            .filter_map(|id| CPU_SETS.iter().find(|e| e.id == *id).map(|e| (e.group, e.lp_index)))
            .collect();
        Ok(locations)
    }
}

// =========================================================================
// Unified scheduling-mode entry (strict hard mask / soft CPU Sets)
// =========================================================================

/// Apply affinity using the requested scheduling mode. Returns whether
/// the soft mode was actually applied.
///
/// In soft mode, if the system does not support CPU Sets (pre-Win10 1803)
/// this automatically falls back to the hard mask - the single-process
/// GUI setter and the rule engine both go through here so the fallback
/// strategy only lives in one place.
pub fn set_affinity_by_mode(pid: u32, mask: u64, mode: crate::rule::RuleMode) -> Result<bool, String> {
    set_affinity_by_group_masks(pid, &[mask], mode)
}

/// Apply either strict group affinity or cross-group CPU Sets. On systems
/// without CPU Sets, soft mode falls back to the strict group implementation.
pub fn set_affinity_by_group_masks(pid: u32, masks: &[u64], mode: crate::rule::RuleMode) -> Result<bool, String> {
    validate_group_masks(masks)?;
    match mode {
        crate::rule::RuleMode::Strict => {
            set_process_affinity_group_masks(pid, masks)?;
            Ok(false)
        }
        crate::rule::RuleMode::Soft => match set_process_soft_affinity_group_masks(pid, masks) {
            Ok(()) => Ok(true),
            Err(e) => {
                if cpu_sets_available() {
                    Err(e)
                }
                // Pre-Win10 1803: no CPU Sets, fall back to hard mask.
                else {
                    set_process_affinity_group_masks(pid, masks)?;
                    Ok(false)
                }
            }
        },
    }
}

#[cfg(test)]
mod group_mask_tests {
    use super::validate_group_masks_for_counts;

    #[test]
    fn accepts_masks_within_each_group_range() {
        assert!(validate_group_masks_for_counts(&[0x8000_0000_0000_0001, 0x3], &[64, 2]).is_ok());
    }

    #[test]
    fn rejects_bits_outside_a_short_group() {
        let error = validate_group_masks_for_counts(&[0x1, 0x4], &[64, 2]).unwrap_err();
        assert!(error.contains("group 1"));
    }

    #[test]
    fn rejects_empty_selection_and_extra_groups() {
        assert!(validate_group_masks_for_counts(&[0, 0], &[64, 4]).is_err());
        assert!(validate_group_masks_for_counts(&[1, 1], &[64]).is_err());
    }

    #[test]
    fn cpu_set_mapping_keeps_same_lp_index_in_separate_groups_distinct() {
        let entries = [
            super::CpuSetEntry { group: 0, lp_index: 1, id: 101 },
            super::CpuSetEntry { group: 1, lp_index: 1, id: 201 },
            super::CpuSetEntry { group: 1, lp_index: 2, id: 202 },
        ];
        assert_eq!(
            super::cpu_set_ids_for_group_masks_from_entries(&[0b10, 0], &entries),
            vec![101]
        );
        assert_eq!(
            super::cpu_set_ids_for_group_masks_from_entries(&[0, 0b110], &entries),
            vec![201, 202]
        );
    }
}

// =========================================================================
// Lightweight process enumeration (used by the rule engine; no metrics)
// =========================================================================

/// Process entry needed by the rule engine.
pub struct ProcessEntry {
    pub pid: u32,
    /// Executable file name (e.g. "codex.exe").
    pub name: String,
    /// Full executable path (only resolved when `include_paths=true` and
    /// the resolution succeeded).
    pub path: Option<String>,
}

/// ToolHelp snapshot enumeration; when `include_paths=true`, each entry
/// has its full path resolved via `OpenProcess` + `QueryFullProcessImageNameW`
/// (required by Path-type rules).
pub fn enumerate_processes(include_paths: bool) -> Result<Vec<ProcessEntry>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot failed: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name_len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                let pid = entry.th32ProcessID;
                let path = if include_paths { query_image_path(pid) } else { None };
                processes.push(ProcessEntry { pid, name, path });
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(processes)
    }
}

/// Resolve the full executable path from an already-open handle (Win32 path
/// format; None on failure). Reuses the caller's handle, avoiding a second
/// OpenProcess call.
pub fn query_image_path_from_handle(handle: HANDLE) -> Option<String> {
    unsafe {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result =
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        if result.is_ok() {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// Resolve the full executable path by opening a handle for the PID
/// (None when it can't be opened / is a protected process).
fn query_image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let path = query_image_path_from_handle(handle);
        close(handle);
        path
    }
}

/// Full executable path of a single process (lets the GUI resolve or
/// refresh it on demand, e.g. for the right-click "Copy Path" action).
pub fn get_process_exe_path(pid: u32) -> Option<String> {
    query_image_path(pid)
}

/// Query the process's executable name (last segment of the path; None
/// when the handle cannot be opened / the process has exited). ProBalance
/// uses this before restoring to verify the PID is not reused by a new
/// process: a mismatched name means the original PID has been taken.
pub fn get_process_name(pid: u32) -> Option<String> {
    let path = query_image_path(pid)?;
    path.rsplit(['\\', '/']).next().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_mask_basic() {
        assert_eq!(parse_hex_mask("0xFF").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("FF").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("0xff").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("0xFFFFFFFFFFFFFFFF").unwrap(), u64::MAX);
        assert_eq!(parse_hex_mask("  0xFF  ").unwrap(), 0xFF);
    }

    #[test]
    fn parse_hex_mask_invalid() {
        assert!(parse_hex_mask("").is_err());
        assert!(parse_hex_mask("  ").is_err());
        assert!(parse_hex_mask("0x").is_err());
        assert!(parse_hex_mask("0X").is_err());
        assert!(parse_hex_mask("0xFFFFFFFFFFFFFFFF0").is_err());
        assert!(parse_hex_mask("0xGHIJ").is_err());
        assert!(parse_hex_mask("not_a_hex").is_err());
    }

    #[test]
    fn mask_to_hex_roundtrip() {
        for v in [0xFFu64, 0x1, 0xABCDEF0, u64::MAX] {
            let hex = mask_to_hex(v);
            assert_eq!(parse_hex_mask(&hex).unwrap(), v);
        }
    }

    // ---------- Priority class scheduling rank ----------

    #[test]
    fn priority_class_rank_matches_scheduler_order() {
        // Scheduling order: IDLE < BELOW_NORMAL < NORMAL < ABOVE_NORMAL < HIGH < REALTIME
        let idle = priority_class_rank(0x40).unwrap();
        let below = priority_class_rank(0x4000).unwrap();
        let normal = priority_class_rank(0x20).unwrap();
        let above = priority_class_rank(0x8000).unwrap();
        let high = priority_class_rank(0x80).unwrap();
        let realtime = priority_class_rank(0x100).unwrap();
        assert!(idle < below && below < normal && normal < above && above < high && high < realtime);
    }

    #[test]
    fn priority_class_rank_rejects_unknown_values() {
        // Numerically larger than BELOW_NORMAL(0x4000) but not a valid
        // priority class.
        assert!(priority_class_rank(0x5000).is_none());
        // EcoQoS background mode flags / combination values -> unknown,
        // conservatively rejected.
        assert!(priority_class_rank(0x0010_0000).is_none());
        assert!(priority_class_rank(0).is_none());
    }
}
