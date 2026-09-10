//! CPU topology detection (CCD-aware edition).
//!
//! On AMD Zen desktop parts (especially multi-CCD chips like the Ryzen
//! 7950X/9950X), Windows does not normally expose the CCD structure through
//! `GetLogicalProcessorInformationEx`'s `RelationProcessorDie`. Tools such as
//! Process Lasso, Ryzen Master, and LibreHardwareMonitor use the CPUID
//! instruction to identify CCDs precisely: pin a thread to each logical
//! processor, then execute `CPUID EAX=0x8000_001E` (AMD Processor Topology
//! Enumeration leaf, Family 17h+), and read the `ECX[7:0]` field to obtain
//! the Node_ID (= CCD index).
//!
//! This module detects Die/CCD in the following priority order:
//!   1. CPUID `Fn8000_001E` thread-pinning method (primary, same as Process
//!      Lasso, Node ID = CCD).
//!   2. CPUID `Fn8000_0026` (Die ID fallback, needed only for some older
//!      Family 19h SKUs).
//!   3. `RelationProcessorModule` = 9 (new in Win11; some AMD configurations
//!      tag CCDs as Modules).
//!   4. `RelationProcessorDie` (original approach, essentially only emitted
//!      by Server SKUs).
//!   5. Fall back to a single logical Die.

use std::mem::size_of;
use std::path::{Path, PathBuf};

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::__cpuid_count;

#[allow(unused_imports)]
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemInfo, LOGICAL_PROCESSOR_RELATIONSHIP, RelationAll,
    RelationGroup, RelationNumaNode, RelationProcessorCore, RelationProcessorDie,
    RelationProcessorPackage, SYSTEM_INFO, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetProcessAffinityMask,
    SetThreadAffinityMask,
};

use crate::models::{CoreInfo, CpuTopology, DieInfo, LogicalProcessorInfo};

/// SMT flag bit (LTP_PC_SMT).
const LTP_PC_SMT: u8 = 0x1;

/// Raw values of known LOGICAL_PROCESSOR_RELATIONSHIP enumerations; some of
/// these are not yet stable in the windows crate.
const RELATION_NUMA_NODE: i32 = 1;
const RELATION_PROCESSOR_CACHE: i32 = 4;
/// RelationProcessorModule = 9 (introduced in Win11 22H2+; windows crate has
/// not yet exposed this enum value).
const RELATION_PROCESSOR_MODULE: i32 = 9;

/// CPUID maximum extended function leaf (returned by EAX=0x8000_0000).
const CPUID_MAX_EXT_LEAF: u32 = 0x8000_0000;
/// AMD extended topology leaf 1: Processor Topology Enumeration (Family 17h+,
/// all Ryzen).
///  - ECX[7:0]   = Node_ID (= CCD index; the most reliable CCD source for
///                 desktop Ryzen, same as Process Lasso).
///  - ECX[10:8]  = NodesPerProcessor - 1.
const CPUID_AMD_TOPOLOGY_ENUM: u32 = 0x8000_001E;
/// AMD extended topology leaf 2: Extended APIC ID (some older Family 19h SKUs
/// use ECX[15:8] = Die_ID).
const CPUID_AMD_EXT_TOPOLOGY: u32 = 0x8000_0026;
/// CPUID Vendor ID leaf.
const CPUID_VENDOR: u32 = 0x0000_0000;

// ============================================================
//   Public main entry point
// ============================================================

pub fn get_cpu_topology() -> Result<CpuTopology, String> {
    let buffer = query_logical_processor_info()?;

    // ---------- Step 1: Parse Core / Package / Module / Die relations ----------
    let mut core_entries: Vec<(u16, u64, u8, u8)> = Vec::new(); // (group, mask, flags, efficiency)
    let mut package_entries: Vec<(u16, u64)> = Vec::new();
    let mut die_candidates_winapi: Vec<(u16, u64)> = Vec::new();
    // RelationGroup is the topology API's authoritative count for active
    // Windows processor groups. Keep the GetActiveProcessorGroupCount call
    // only as a defensive fallback for malformed/provider-limited buffers.
    let mut relation_group_count: Option<u16> = None;

    let mut offset = 0usize;
    while offset + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buffer.len() {
        let entry_ptr = buffer.as_ptr().wrapping_add(offset)
            as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        let entry = unsafe { &*entry_ptr };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buffer.len() {
            break;
        }

        let rel = entry.Relationship.0;

        if rel == RelationGroup.0 {
            let group_info = unsafe { &entry.Anonymous.Group };
            if group_info.ActiveGroupCount > 0 {
                relation_group_count = Some(group_info.ActiveGroupCount);
            }
        }

        if rel == RelationProcessorCore.0
            || rel == RelationProcessorPackage.0
            || rel == RelationProcessorDie.0
            || rel == RELATION_PROCESSOR_MODULE
        {
            // PROCESSOR_RELATIONSHIP ends in a variable-length GroupMask
            // array. Keep a reference into the original API buffer rather
            // than copying the one-element Rust projection.
            let proc_info = unsafe { &entry.Anonymous.Processor };
            if proc_info.GroupCount >= 1 {
                for gi in 0..proc_info.GroupCount as usize {
                    let group_mask = unsafe { *proc_info.GroupMask.as_ptr().add(gi) };
                    let group = group_mask.Group;
                    let mask = group_mask.Mask as u64;
                    if rel == RelationProcessorCore.0 {
                        core_entries.push((group, mask, proc_info.Flags, proc_info.EfficiencyClass));
                    } else if rel == RelationProcessorPackage.0 {
                        package_entries.push((group, mask));
                    } else if !die_candidates_winapi.iter().any(|&(g, m)| g == group && m == mask) {
                        die_candidates_winapi.push((group, mask));
                    }
                }
            }
        }

        offset += entry_size;
    }

    if core_entries.is_empty() {
        return Err("no processor core information detected".to_string());
    }

    // ---------- Step 2: CPUID-based die_id per logical processor ----------
    // ★Key point★: detect_die_by_cpuid calls SetThreadAffinityMask once per LP,
    // which mutates the current thread's affinity. Running it directly on a
    // Tauri command thread will disrupt the tokio async runtime and freeze
    // the whole app, so it must be executed in a dedicated thread that we
    // then join.
    let total_lps_guess: u32 = core_entries.iter().map(|(_, m, _, _)| m.count_ones()).sum();
    let lp_die_from_cpuid: Option<Vec<u32>> = if cpum_core::procwin::active_group_count() > 1 { None } else { std::thread::scope(|s| {
        let handle = s.spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                detect_die_by_cpuid(total_lps_guess)
            }))
            .unwrap_or_else(|_| None)
        });
        handle.join().unwrap_or(None)
    }) };

    let mut core_die_ids: Vec<u32> = Vec::with_capacity(core_entries.len());

    if let Some(lp2die) = &lp_die_from_cpuid {
        for &(_, mask, _, _) in &core_entries {
            let mut assigned: Option<u32> = None;
            for bit in 0..64u32 {
                if (mask >> bit) & 1 == 1 {
                    if let Some(&die_id) = lp2die.get(bit as usize) {
                        assigned = Some(die_id);
                        break;
                    }
                }
            }
            core_die_ids.push(assigned.unwrap_or(0));
        }
    } else {
        for &(group, mask, _, _) in &core_entries {
            let die_id = die_candidates_winapi
                .iter()
                .position(|&(candidate_group, dm)| group == candidate_group && mask & dm == mask)
                .map(|p| p as u32)
                .unwrap_or(0);
            core_die_ids.push(die_id);
        }
    }

    // ---------- Step 3: Renumber logical Die IDs (compact 0..N-1) ----------
    let unique_die_ids: Vec<u32> = {
        let mut v = core_die_ids.clone();
        v.sort_unstable();
        v.dedup();
        v
    };
    let compact_die_of = |raw_id: u32| -> u32 {
        unique_die_ids
            .iter()
            .position(|&x| x == raw_id)
            .unwrap_or(0) as u32
    };

    // ---------- Step 4: Build core / lp / die output ----------
    let mut logical_processors: Vec<LogicalProcessorInfo> = Vec::new();
    let mut cores: Vec<CoreInfo> = Vec::new();
    let n_dies = unique_die_ids.len().max(1);
    let mut die_threads: Vec<Vec<u32>> = vec![Vec::new(); n_dies];
    let mut die_cores: Vec<Vec<u32>> = vec![Vec::new(); n_dies];

    for (core_idx, &(group, mask, flags, eff)) in core_entries.iter().enumerate() {
        let core_id = core_idx as u32;
        let raw_die = core_die_ids[core_idx];
        let die_id = compact_die_of(raw_die);

        let package_id = package_entries
            .iter()
            .position(|&(package_group, pmask)| package_group == group && mask & pmask == mask)
            .map(|p| p as u32)
            .unwrap_or(0);

        let has_smt = (flags & LTP_PC_SMT) != 0;
        let mut threads: Vec<u32> = Vec::new();
        let mut smt_thread_id = 0u32;
        for bit in 0..64u32 {
            if (mask >> bit) & 1 == 1 {
                let global_index = group as u32 * 64 + bit;
                threads.push(global_index);
                die_threads[die_id as usize].push(global_index);
                logical_processors.push(LogicalProcessorInfo {
                    index: global_index,
                    group,
                    group_index: bit as u8,
                    core_id,
                    die_id,
                    package_id,
                    smt_thread_id,
                    efficiency_class: eff,
                    is_smt_secondary: has_smt && smt_thread_id > 0,
                });
                smt_thread_id += 1;
            }
        }
        die_cores[die_id as usize].push(core_id);

        cores.push(CoreInfo {
            id: core_id,
            die_id,
            package_id,
            has_smt,
            threads,
            efficiency_class: eff,
        });
    }

    let multi_die = n_dies >= 2;
    let dies: Vec<DieInfo> = (0..n_dies as u32)
        .map(|die_id| {
            // A die can span a processor group boundary on large systems;
            // do not collapse its global LP indices into one u64 here. The
            // owning cores already carry the group-aware package mapping.
            let package_id = die_cores[die_id as usize]
                .first()
                .and_then(|core_id| cores.get(*core_id as usize))
                .map(|core| core.package_id)
                .unwrap_or(0);
            DieInfo {
                id: die_id,
                package_id,
                cores: die_cores[die_id as usize].clone(),
                threads: die_threads[die_id as usize].clone(),
                is_ccd: multi_die,
            }
        })
        .collect();

    let total_logical_processors = logical_processors.len() as u32;
    let group_count = relation_group_count
        .unwrap_or_else(cpum_core::procwin::active_group_count);
    Ok(CpuTopology {
        logical_processors,
        cores,
        dies,
        total_logical_processors,
        group_count,
        single_group: group_count <= 1,
    })
}

// ============================================================
//   CPUID thread-pinning method
// ============================================================

/// Get the true global usable affinity mask (system_mask) of the current system.
///
/// ⚠️ Critical fix (Ryzen 9000 32-LP scenario):
/// Some tools (Process Lasso / launcher / parent process) may **restrict the
/// process-level** affinity mask to the lower 16 LPs. In that state, calling
/// `SetThreadAffinityMask(thread, 1 << 16..=31)` returns 0 (failure) because
/// the **thread-level mask cannot exceed the process-level mask**.
///
/// Workaround: explicitly expand the current process's affinity mask to
/// `system_mask` first (Win32 allows this without admin rights); only then can
/// we pin to LPs 16-31. Returns `(original_process_mask, system_mask)` so the
/// caller can restore the process mask afterwards.
fn expand_process_affinity_to_system() -> Option<(usize, usize)> {
    unsafe {
        let mut process_mask: usize = 0;
        let mut system_mask: usize = 0;
        GetProcessAffinityMask(GetCurrentProcess(), &mut process_mask, &mut system_mask)
            .ok()?;
        if system_mask == 0 {
            return None;
        }
        // First try to expand the process-level mask to the full set of
        // usable LPs. If the system disallows it, fall back to the original
        // process_mask.
        if process_mask != system_mask {
            let _ = SetProcessAffinityMask(GetCurrentProcess(), system_mask);
            // Read it again to confirm what actually took effect
            let mut pm: usize = 0;
            let mut sm: usize = 0;
            if GetProcessAffinityMask(GetCurrentProcess(), &mut pm, &mut sm).is_ok() {
                return Some((process_mask, pm)); // The actual system_mask is the new pm
            }
        }
        Some((process_mask, system_mask))
    }
}

/// Restore the process affinity mask (optional helper).
#[allow(dead_code)]
fn restore_process_affinity(original: usize) {
    unsafe {
        let _ = SetProcessAffinityMask(GetCurrentProcess(), original);
    }
}

/// Try to identify each logical processor's Die_ID via CPUID.
/// Method priority (aligned with Process Lasso / LibreHardwareMonitor):
///   1. Fn8000_001E (Processor Topology Enum) -> ECX[7:0] = Node_ID = CCD
///      (most reliable source for multi-CCD desktop Ryzen 7950X / 9950X, etc.).
///   2. Fn8000_0026 (Extended APIC ID)        -> ECX[15:8] = Die_ID
///      (some older Family 19h SKUs; fall back here if Node_ID is all zero).
///   3. Fn8000_001E EAX = x2APIC ID -> find the highest bit that splits the
///      population into two roughly equal buckets (some BIOSes zero out
///      Node_ID but still partition via x2APIC high bits, e.g. 9950X with
///      CCD0 = APIC 0..15 / CCD1 = APIC 16..31).
/// Returns `None` to signal "fall back to WinAPI Die candidates".
fn detect_die_by_cpuid(total_lps_guess: u32) -> Option<Vec<u32>> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }

    let vendor = unsafe { cpuid_vendor() };
    if vendor.as_str() != "AuthenticAMD" {
        return None;
    }
    let max_ext = unsafe { cpuid_max_ext_leaf() };

    let total = total_lps_guess as usize;
    if total == 0 || total > 64 {
        return None;
    }

    let thread = unsafe { GetCurrentThread() };
    // First expand the process-level affinity to system_mask (to escape any
    // low-16-LP limit set by the parent / launcher).
    let (orig_proc_mask, sys_mask) = match expand_process_affinity_to_system() {
        Some(pair) => pair,
        None => return None,
    };
    let old_affinity = unsafe { SetThreadAffinityMask(thread, sys_mask) };
    if old_affinity == 0 {
        // Even if the thread-level mask can't be set back, try to restore the
        // process-level mask
        restore_process_affinity(orig_proc_mask);
        return None;
    }
    let orig_proc_for_cleanup = Some(orig_proc_mask);

    // ------------------------------------------------------------------
    // One round of pinning reads all three candidate fields at once, to
    // avoid repeated SetThreadAffinityMask + thread migration.
    // For each LP we get: (node_id, die_id_26, x2apic_id)
    // ------------------------------------------------------------------
    struct PerLp {
        node_id: u32,    // Fn001E ECX[7:0]
        die_id_26: u32,  // Fn0026 ECX[15:8]
        x2apic: u32,     // Fn001E EAX
        ok: bool,        // Whether pinning succeeded
    }
    let mut per_lp: Vec<PerLp> = (0..total)
        .map(|_| PerLp { node_id: 0, die_id_26: 0, x2apic: 0, ok: false })
        .collect();

    for lp in 0..total {
        let pin: usize = 1usize << lp;
        let r = unsafe { SetThreadAffinityMask(thread, pin) };
        if r == 0 {
            continue;
        }
        // Wait for the OS to actually migrate us; a successful pin does not
        // guarantee the thread is already running on that core. Use a
        // microsecond sleep instead of yield_now to avoid scheduler storms
        // inside the Tauri thread pool.
        std::thread::sleep(std::time::Duration::from_micros(200));

        let mut node_vals = [0u32; 2];
        let mut die_vals = [0u32; 2];
        let mut x2apic_vals = [0u32; 2];
        for i in 0..2usize {
            std::thread::sleep(std::time::Duration::from_micros(100));
            let (eax_1e, _, ecx_1e, _) = unsafe { cpuid_leaf(CPUID_AMD_TOPOLOGY_ENUM, 0) };
            let (_, _, ecx_26, _) = unsafe { cpuid_leaf(CPUID_AMD_EXT_TOPOLOGY, 0) };
            node_vals[i] = ecx_1e & 0xFF;
            die_vals[i] = (ecx_26 >> 8) & 0xFF;
            x2apic_vals[i] = eax_1e;
        }
        per_lp[lp] = PerLp {
            node_id: node_vals[0],
            die_id_26: die_vals[0],
            x2apic: x2apic_vals[0],
            ok: true,
        };
    }
    let restore = || unsafe { SetThreadAffinityMask(thread, old_affinity); };

    // ---------- Method 1: Fn8000_001E ECX[7:0] = Node_ID ----------
    if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
        let mut result: Vec<u32> = vec![u32::MAX; total];
        let mut any_different = false;
        let mut last: Option<u32> = None;
        for lp in 0..total {
            if !per_lp[lp].ok {
                continue;
            }
            let node = per_lp[lp].node_id;
            result[lp] = node;
            if let Some(p) = last {
                if p != node {
                    any_different = true;
                }
            }
            last = Some(node);
        }
        if any_different {
            restore();
            return Some(result);
        }
    }

    // ---------- Method 2: Fn8000_0026 ECX[15:8] = Die_ID ----------
    if max_ext >= CPUID_AMD_EXT_TOPOLOGY {
        let mut result: Vec<u32> = vec![u32::MAX; total];
        let mut any_different = false;
        let mut last: Option<u32> = None;
        for lp in 0..total {
            if !per_lp[lp].ok {
                continue;
            }
            let die = per_lp[lp].die_id_26;
            result[lp] = die;
            if let Some(p) = last {
                if p != die {
                    any_different = true;
                }
            }
            last = Some(die);
        }
        if any_different {
            restore();
            return Some(result);
        }
    }

    // ---------- Method 3: x2APIC high-bit bucketing (BIOS hides Node_ID
    // but still partitions via x2APIC) ----------
    // Enabled only when it "looks like a multi-CCD desktop Ryzen": at least
    // 16 LPs, and the x2APIC set's max - min + 1 == number of successfully
    // probed LPs (which means x2APIC is contiguously numbered - the desktop
    // Ryzen layout).
    if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
        let ok_lps: Vec<usize> = (0..total).filter(|&lp| per_lp[lp].ok).collect();
        if ok_lps.len() >= 16 {
            let mut xs: Vec<u32> = ok_lps.iter().map(|&lp| per_lp[lp].x2apic).collect();
            xs.sort_unstable();
            xs.dedup();
            let min_x = xs.first().copied().unwrap_or(0);
            let max_x = xs.last().copied().unwrap_or(0);
            let span = (max_x - min_x + 1) as usize;
            let unique = xs.len();
            // Contiguous (span == unique) and span >= 16 -> looks like 2-CCD desktop layout
            if span == unique && span >= 16 {
                // Find the highest bit such that its 0/1 buckets each hold at
                // least 25% (avoids being fooled by SMT's lowest bit).
                let highest_bit = 31u32.saturating_sub(span.leading_zeros());
                let mut bucket_bit = None;
                for b in (1..=highest_bit).rev() {
                    let mut zeros = 0usize;
                    let mut ones = 0usize;
                    for lp in ok_lps.iter().copied() {
                        if (per_lp[lp].x2apic >> b) & 1 == 0 {
                            zeros += 1;
                        } else {
                            ones += 1;
                        }
                    }
                    let threshold = ok_lps.len() / 4; // 25%
                    if zeros >= threshold && ones >= threshold {
                        bucket_bit = Some(b);
                        break;
                    }
                }
                if let Some(bit) = bucket_bit {
                    let mut result: Vec<u32> = vec![u32::MAX; total];
                    for lp in ok_lps.iter().copied() {
                        result[lp] = (per_lp[lp].x2apic >> bit) & 1;
                    }
                    restore();
                    if let Some(opm) = orig_proc_for_cleanup {
                        restore_process_affinity(opm);
                    }
                    return Some(result);
                }
            }
        }
    }

    // ---------- Method 4: Even LP split (last resort - matches 9950X 2CCD = LP 0-15/16-31) ----------
    // On desktop Ryzen, the Win32 ProcessorCore mask order is:
    // Core0 -> LP 0,1  Core1 -> LP 2,3 ...
    // i.e. LP 0..N/2 on CCD0 and LP N/2..N on CCD1. If the number of LPs is
    // a power of two (16/32/64) or an exact multiple of two with both halves
    // >= 8, just split evenly.
    {
        let ok_lps: Vec<usize> = (0..total).filter(|&lp| per_lp[lp].ok).collect();
        // We don't strictly require every LP to pin successfully; we just
        // need at least 16, or coverage of 80%+ of the total.
        let ok_count = ok_lps.len();
        if total >= 16 && total % 2 == 0 && ok_count.max(1) * 5 >= total * 4 {
            let half = total / 2;
            // Each half must hold at least 8 LPs (avoid splitting a 16-core
            // single-CCD chip in two).
            if half >= 8 {
                let mut result: Vec<u32> = vec![0u32; total];
                for lp in 0..total {
                    result[lp] = if lp < half { 0 } else { 1 };
                }
                restore();
                if let Some(opm) = orig_proc_for_cleanup {
                    restore_process_affinity(opm);
                }
                return Some(result);
            }
        }
    }

    restore();
    if let Some(opm) = orig_proc_for_cleanup {
        restore_process_affinity(opm);
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn cpuid_leaf(leaf: u32, sub_leaf: u32) -> (u32, u32, u32, u32) {
    let res = __cpuid_count(leaf, sub_leaf);
    (res.eax, res.ebx, res.ecx, res.edx)
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_leaf(_leaf: u32, _sub_leaf: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}

#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_vendor() -> String {
    let (_, ebx, ecx, edx) = unsafe { cpuid_leaf(CPUID_VENDOR, 0) };
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&ebx.to_le_bytes());
    bytes.extend_from_slice(&edx.to_le_bytes());
    bytes.extend_from_slice(&ecx.to_le_bytes());
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_vendor() -> String {
    String::new()
}

#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_max_ext_leaf() -> u32 {
    let (eax, _, _, _) = unsafe { cpuid_leaf(CPUID_MAX_EXT_LEAF, 0) };
    eax
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_max_ext_leaf() -> u32 {
    0
}

// ============================================================
//   Win32 low-level calls
// ============================================================

fn query_logical_processor_info() -> Result<Vec<u8>, String> {
    let mut len: u32 = 0;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationAll, None, &mut len);
    }
    if len == 0 {
        return Err("GetLogicalProcessorInformationEx returned length 0".to_string());
    }
    let mut buffer: Vec<u8> = vec![0u8; len as usize];
    let result = unsafe {
        GetLogicalProcessorInformationEx(
            RelationAll,
            Some(buffer.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )
    };
    result.map_err(|e| format!("GetLogicalProcessorInformationEx failed: {}", e))?;
    Ok(buffer)
}

// ============================================================
//   Debug helper: dump raw Win32 topology + CPUID results
// ============================================================

#[allow(dead_code)]
pub fn dump_raw_topology() -> String {
    let buffer = match query_logical_processor_info() {
        Ok(b) => b,
        Err(e) => return format!("[ERROR] {}", e),
    };

    let mut out = String::new();
    out.push_str("=== Win32 GetLogicalProcessorInformationEx(RelationAll) Dump ===\n");
    out.push_str(&format!("Total bytes: {}\n\n", buffer.len()));

    let mut offset = 0usize;
    while offset + size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buffer.len() {
        let entry_ptr = buffer.as_ptr().wrapping_add(offset)
            as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
        let entry = unsafe { &*entry_ptr };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buffer.len() {
            break;
        }

        let rel = entry.Relationship.0;
        let rel_name: &'static str = match rel {
            r if r == RelationProcessorCore.0 => "ProcessorCore",
            r if r == RelationProcessorPackage.0 => "ProcessorPackage",
            r if r == RelationProcessorDie.0 => "ProcessorDie",
            r if r == RELATION_PROCESSOR_MODULE => "ProcessorModule(=9)",
            r if r == RELATION_NUMA_NODE => "NumaNode(=1)",
            r if r == RELATION_PROCESSOR_CACHE => "ProcessorCache(=4)",
            _ => "Other",
        };

        out.push_str(&format!(
            "[{:#06x}] Relationship = {} (raw {})\n",
            offset, rel_name, rel
        ));
        out.push_str(&format!("  Size          = {} bytes\n", entry_size));

        let is_proc_kind = rel == RelationProcessorCore.0
            || rel == RelationProcessorPackage.0
            || rel == RelationProcessorDie.0
            || rel == RELATION_PROCESSOR_MODULE;

        if is_proc_kind {
            let p = unsafe { entry.Anonymous.Processor };
            out.push_str(&format!("  GroupCount    = {}\n", p.GroupCount));
            for gi in 0..p.GroupCount.min(1) as usize {
                let gm = p.GroupMask[gi];
                let mask = gm.Mask as u64;
                out.push_str(&format!(
                    "  Group[{}].Mask = 0x{:016X} ({} bits)\n",
                    gi,
                    mask,
                    mask.count_ones()
                ));
                out.push_str(&format!("  Group[{}].Group= {}\n", gi, gm.Group));
            }
            if rel == RelationProcessorCore.0 {
                out.push_str(&format!("  Flags         = {:#x}\n", p.Flags));
                out.push_str(&format!("  EfficiencyClass = {}\n", p.EfficiencyClass));
            }
        }

        out.push('\n');
        offset += entry_size;
    }

    // ---- CPUID debug information ----
    out.push_str("\n=== CPUID (AMD extended topology leaf) ===\n");
    if !cfg!(target_arch = "x86_64") {
        out.push_str("Not x86_64, skipped.\n");
    } else {
        let vendor = unsafe { cpuid_vendor() };
        let max_ext = unsafe { cpuid_max_ext_leaf() };
        out.push_str(&format!("Vendor         : {}\n", vendor));
        out.push_str(&format!("Max ext leaf   : 0x{:08X}\n", max_ext));
        if max_ext >= CPUID_AMD_TOPOLOGY_ENUM {
            out.push_str("Leaf 0x8000001E: supported (Node ID in ECX[7:0], aka CCD)\n");
        } else {
            out.push_str("Leaf 0x8000001E: NOT supported by this CPU / BIOS.\n");
        }
        if max_ext >= CPUID_AMD_EXT_TOPOLOGY {
            out.push_str("Leaf 0x80000026: supported (Die ID in ECX[15:8], fallback)\n");
        } else {
            out.push_str("Leaf 0x80000026: NOT supported by this CPU / BIOS.\n");
        }

        // ================================================================
        // Regardless of whether multi-die was detected, force-print the full
        // per-LP triple (Node/x2APIC/Die) so that whether the BIOS hides
        // Node_ID or the pinning is flaky, the raw values can be inspected.
        // ================================================================
        let total_probe = 32usize;
        out.push_str(&format!(
            "\n[Per-LP CPUID raw values, probe LP 0..{}] (pinning + 2x samples)\n",
            total_probe - 1
        ));
        out.push_str("  LP : Node_ID  Die_ID26  x2APIC  pin?\n");
        out.push_str("  ------------------------------------\n");

        let thread = unsafe { GetCurrentThread() };
        // First expand the process-level mask; otherwise LP 16-31 pinning
        // will all fail due to the restricted process mask.
        let (orig_proc_mask, sys_mask) = expand_process_affinity_to_system()
            .unwrap_or((0xFFFF_FFFF, 0xFFFF_FFFF));
        let old = unsafe { SetThreadAffinityMask(thread, sys_mask) };
        if old != 0 {
            for lp in 0..total_probe {
                let pin = 1usize << lp;
                let r = unsafe { SetThreadAffinityMask(thread, pin) };
                if r == 0 {
                    out.push_str(&format!("  {:>2}: <pinning failed>\n", lp));
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
                let (eax_1e, _, ecx_1e, _) = unsafe { cpuid_leaf(CPUID_AMD_TOPOLOGY_ENUM, 0) };
                let (_, _, ecx_26, _) = unsafe { cpuid_leaf(CPUID_AMD_EXT_TOPOLOGY, 0) };
                let node = ecx_1e & 0xFF;
                let die26 = (ecx_26 >> 8) & 0xFF;
                let x2apic = eax_1e;
                out.push_str(&format!(
                    "  {:>2}:    {:>2}       {:>2}       {:>3}    OK\n",
                    lp, node, die26, x2apic
                ));
            }
            unsafe {
                SetThreadAffinityMask(thread, old);
            }
            restore_process_affinity(orig_proc_mask);
        }

        // ---- Run the real detect once and print the result ----
        out.push_str("\n[detect_die_by_cpuid(32) final result]\n");
        match detect_die_by_cpuid(total_probe as u32) {
            Some(map) => {
                let mut unique: Vec<u32> = map.iter().copied().filter(|&x| x != u32::MAX).collect();
                unique.sort_unstable();
                unique.dedup();
                out.push_str(&format!(
                    "  -> Multi-die detected, {} distinct Die IDs. Per-LP mapping:\n",
                    unique.len()
                ));
                for (lp, &die) in map.iter().enumerate() {
                    if die != u32::MAX {
                        out.push_str(&format!("  LP {:>2} -> Die {}\n", lp, die));
                    }
                }
            }
            None => {
                out.push_str("  -> Returned None (giving up on multi-CCD detection, falling back to WinAPI / single Die).\n");
            }
        }
    }

    // ---- Final: call get_cpu_topology() once and print the Die/threads list the frontend actually uses ----
    out.push_str("\n=== Final CpuTopology::dies mapping (what frontend actually uses) ===\n");
    match get_cpu_topology() {
        Ok(topo) => {
            out.push_str(&format!(
                "total_logical_processors = {}, total cores = {}, dies = {}\n",
                topo.total_logical_processors,
                topo.cores.len(),
                topo.dies.len()
            ));
            for die in topo.dies.iter() {
                out.push_str(&format!(
                    "  Die {} (package={}, is_ccd={}): threads={:?}, cores={:?}\n",
                    die.id, die.package_id, die.is_ccd, die.threads, die.cores
                ));
            }
        }
        Err(e) => {
            out.push_str(&format!("  [ERROR] get_cpu_topology failed: {}\n", e));
        }
    }

    // ================================================================
    // Write the full diagnostic to <cwd>\cpum-topology-dump.txt so the
    // user can one-click copy it to the developer.
    // ================================================================
    let save_path: Option<std::path::PathBuf> = (|| {
        let dir = std::env::current_dir().ok()?;
        let path = dir.join("cpum-topology-dump.txt");
        std::fs::write(&path, &out).ok()?;
        Some(path)
    })();
    match save_path {
        Some(p) => out.push_str(&format!(
            "\n\n[diagnostic auto-saved] full report written to: {}\n",
            p.display()
        )),
        None => out.push_str("\n\n[diagnostic not saved] auto-write to current directory failed; please copy the text above manually.\n"),
    }

    out
}

// ============================================================
//   Topology cache (faster startup - CPU topology almost never changes,
//   only invalidated when the CPU is replaced)
// ============================================================

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct TopologyCache {
    /// Hardware signature: a change means the CPU or motherboard was swapped
    /// and the cache is invalid.
    pub hw_signature: String,
    /// Unix seconds when the cache was written (used for expiry, currently
    /// not enforced - informational only).
    pub ts_secs: u64,
    /// The actual topology data.
    pub topology: CpuTopology,
}

/// Extract the family/model from CPUID Processor Info Leaf (EAX=1).
/// This is a generic x86/x86_64 field, not Intel/AMD-specific.
#[cfg(target_arch = "x86_64")]
unsafe fn cpuid_family_model() -> (u32, u32) {
    // Real family = BaseFamily + (ExtendedFamily if BaseFamily==0Fh else 0)
    let (eax, _, _, _) = unsafe { cpuid_leaf(1, 0) };
    let base_family = (eax >> 8) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let family = if base_family == 0x0F {
        base_family + ext_family
    } else {
        base_family
    };
    let base_model = (eax >> 4) & 0xF;
    let ext_model = (eax >> 16) & 0xF;
    let model = if base_family == 0x06 || base_family == 0x0F {
        (ext_model << 4) | base_model
    } else {
        base_model
    };
    (family, model)
}
#[cfg(not(target_arch = "x86_64"))]
unsafe fn cpuid_family_model() -> (u32, u32) {
    (0, 0)
}

/// Lightweight hardware signature (no pinning, no SetThreadAffinityMask -> <1ms).
/// Composition: vendor-family-model-total_lps_count.
pub fn hw_signature_fast(total_lps: u32) -> String {
    let vendor = unsafe { cpuid_vendor() };
    let (family, model) = unsafe { cpuid_family_model() };
    format!("{}-{:X}-{:X}-{}", vendor, family, model, total_lps)
}

/// Get the logical processor count via GetSystemInfo (<1ms, no CPUID pinning).
pub fn sys_info_logical_processor_count() -> Option<u32> {
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        if si.dwNumberOfProcessors == 0 {
            None
        } else {
            Some(si.dwNumberOfProcessors)
        }
    }
}

fn topology_cache_path(base_dir: &Path) -> PathBuf {
    base_dir.join("cpu_topology_cache.json")
}

/// Read topology from the cache file. Returns:
///   - Ok(Some(data)): read OK, signature matches.
///   - Ok(None):       file missing / signature mismatch / JSON corrupt
///                     (caller falls through to real detection).
pub fn load_topology_cache(
    base_dir: &Path,
    expected_signature: &str,
) -> Result<Option<TopologyCache>, String> {
    let path = topology_cache_path(base_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read cache failed: {e}"))?;
    let parsed: TopologyCache = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if parsed.hw_signature != expected_signature {
        return Ok(None);
    }
    Ok(Some(parsed))
}

/// Write the topology to the cache. The signature is supplied by the caller
/// (computed via the fast signature) to avoid having the writer re-pin.
pub fn save_topology_cache(
    base_dir: &Path,
    topology: &CpuTopology,
    signature: &str,
) -> Result<(), String> {
    if let Err(e) = std::fs::create_dir_all(base_dir) {
        return Err(format!("failed to create cache directory: {e}"));
    }
    let cache = TopologyCache {
        hw_signature: signature.into(),
        ts_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        topology: topology.clone(),
    };
    let json = serde_json::to_string(&cache).map_err(|e| format!("failed to serialize cache: {e}"))?;
    let path = topology_cache_path(base_dir);
    std::fs::write(&path, json).map_err(|e| format!("failed to write cache: {e}"))?;
    Ok(())
}
