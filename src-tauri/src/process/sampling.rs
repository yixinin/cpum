//! Sampling primitives: handle helpers, single-shot metric reads, and the
//! differential cache (foundation for rate metrics).
//!
//! Snapshot and rate caches are module-globals so that `list_processes` and
//! `collect_metrics_tick` (metrics stream) share the same baseline -
//! whichever runs first establishes the baseline, and the next call uses
//! the other's value as `prev` for the rate diff.

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetProcessIoCounters, GetProcessTimes, OpenProcess, IO_COUNTERS, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ProcessBasicInfo {
    pub working_set_bytes: u64,
    pub cpu_total_ticks: Option<u64>,
    pub disk_read_bytes: Option<u64>,
    pub disk_write_bytes: Option<u64>,
    pub net_in_bytes: Option<u64>,
    pub net_out_bytes: Option<u64>,
}

#[repr(C)]
pub(super) struct ProcessNetworkCounters { pub bytes_in: u64, pub bytes_out: u64 }
pub(super) const PROCESS_NETWORK_IO_COUNTERS_CLASS: u32 = 114;

pub(super) fn open_handle(pid: u32, access: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS) -> Option<HANDLE> {
    unsafe { OpenProcess(access, false, pid).ok() }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ProcessIoCounters {
    pub read_transfer_count: u64,
    pub write_transfer_count: u64,
}

/// Minimum interval between two rate-computing samples; below this, we treat
/// the call as a baseline anchor and don't recompute.
pub const MIN_SAMPLE_INTERVAL_MS: u128 = 250;

#[derive(Copy, Clone, Debug, Default)]
pub struct ProcessSnapshot {
    pub cpu_total_ticks: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub net_in_bytes: u64,
    pub net_out_bytes: u64,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct RateSample {
    pub cpu_percent: f32,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
    pub net_in_bps: u64,
    pub net_out_bps: u64,
}

/// Most recent per-process snapshot. Used as the baseline for the next
/// rate-differential computation.
///
/// `Option<(time, map)>`: `None` means the cache has never been written.
/// `Some` with an empty map is a valid (first-call) state.
pub(super) static SNAPSHOT_CACHE: Lazy<Mutex<Option<(std::time::Instant, HashMap<u32, ProcessSnapshot>)>>> =
    Lazy::new(|| Mutex::new(None));

/// Most recently computed per-process rate. Held here so the metrics stream
/// can fill the wave events without re-locking the snapshot map.
pub(super) static RATE_CACHE: Lazy<Mutex<HashMap<u32, RateSample>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Open a process handle for stats reads.
///
/// Returns `(handle, partially_denied)`. The `partially_denied` flag is
/// `true` when some metrics could not be read (e.g. only the limited
/// information handle was available, and certain counters returned errors).
pub(super) fn open_handle_for_stats(pid: u32) -> (Option<HANDLE>, bool) {
    // PID 0 = Idle / System. Skip opening a handle to avoid noisy errors
    // (it can never be opened anyway).
    if pid == 0 {
        return (None, true);
    }
    unsafe {
        // Try the strongest access mask first; fall back to the limited
        // information handle so we still get a workable (partial) view.
        let h = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid);
        if let Ok(h) = h {
            return (Some(h), false);
        }
        let h2 = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        match h2 {
            Ok(h) => (Some(h), true),
            Err(_) => (None, true),
        }
    }
}

pub(super) fn close_handle(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

pub(super) fn get_number_of_processors() -> u32 {
    use windows::Win32::System::SystemInformation::GetSystemInfo;
    unsafe {
        let mut si = std::mem::zeroed();
        GetSystemInfo(&mut si);
        si.dwNumberOfProcessors
    }
}

/// Single-shot read of all relevant per-process counters via a single
/// `OpenProcess`. Returns `None`s for fields that could not be read (e.g.
/// access denied or unsupported).
pub(super) fn read_metrics_for_handle(
    handle: HANDLE,
) -> ProcessBasicInfo {
    let mut create = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    let _ = unsafe { GetProcessTimes(handle, &mut create, &mut exit, &mut kernel, &mut user) };
    let mut pmc: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    let working_set_bytes = unsafe {
        if K32GetProcessMemoryInfo(handle, &mut pmc as *mut _ as *mut _, std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32).as_bool() { pmc.WorkingSetSize as u64 } else { 0 }
    };
    let mut raw: IO_COUNTERS = unsafe { std::mem::zeroed() };
    let io = unsafe { GetProcessIoCounters(handle, &mut raw).ok().map(|_| ProcessIoCounters { read_transfer_count: raw.ReadTransferCount, write_transfer_count: raw.WriteTransferCount }) };

    // IO counters include all file/pipe/socket IO; the per-process network
    // counters (NtQueryInformationProcess class 114) are read separately on
    // Win11 24H2+ and surface as `net_*_bytes`. On older systems the net
    // struct fields stay at 0.
    let kernel_time_100ns = filetime_u64(kernel);
    let user_time_100ns = filetime_u64(user);
    let cpu_total_ticks = Some(kernel_time_100ns + user_time_100ns);
    let disk_read_bytes = io.as_ref().map(|c| c.read_transfer_count);
    let disk_write_bytes = io.as_ref().map(|c| c.write_transfer_count);
    let (net_in_bytes, net_out_bytes) = read_net_counters_for_handle(handle);

    ProcessBasicInfo {
        working_set_bytes,
        cpu_total_ticks,
        disk_read_bytes,
        disk_write_bytes,
        net_in_bytes: Some(net_in_bytes),
        net_out_bytes: Some(net_out_bytes),
    }
}

fn filetime_u64(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Read per-process network IO counters via NtQueryInformationProcess.
/// Win11 24H2+ exposes class 114 (ProcessNetworkIoCounters). Returns
/// (BytesIn, BytesOut); 0 / (0, 0) when unsupported.
fn read_net_counters_for_handle(handle: HANDLE) -> (u64, u64) {
    use windows::Win32::System::Threading::GetProcessId;

    use crate::process::net_probe;

    let pid = unsafe { GetProcessId(handle) };
    if pid == 0 {
        return (0, 0);
    }
    net_probe::read_network_io(pid).unwrap_or((0, 0))
}
