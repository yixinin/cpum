//! Network IO diagnostic probe: verifies whether
//! `NtQueryInformationProcess(ProcessNetworkIoCounters=114)` actually
//! returns data on the current Windows version (Win11 24H2+ only).
//!
//! Used to diagnose "the network column shows 0":
//!  - ntdll resolution failed -> ntdll load issue
//!  - all calls return STATUS_INVALID_INFO_CLASS -> the current Windows
//!    version does not support it
//!  - STATUS_SUCCESS but everything is 0 -> no actual network activity
//!  - non-zero values -> the backend is working, the issue is on the
//!    frontend

use std::mem::size_of;

use windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION;

use cpum_core::procwin::nt_query_information_process;

use super::enumerate::list_basic_entries;
use super::sampling::{
    close_handle, open_handle, ProcessNetworkCounters, PROCESS_NETWORK_IO_COUNTERS_CLASS,
};

pub fn read_network_io(pid: u32) -> Option<(u64, u64)> {
    let handle = open_handle(pid, PROCESS_QUERY_INFORMATION)?;
    let nt = nt_query_information_process()?;
    let mut counters: ProcessNetworkCounters = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    let status = unsafe { nt(handle, PROCESS_NETWORK_IO_COUNTERS_CLASS, &mut counters as *mut _ as *mut std::ffi::c_void, size_of::<ProcessNetworkCounters>() as u32, &mut returned) };
    close_handle(handle);
    (status == 0 && returned as usize >= size_of::<ProcessNetworkCounters>()).then_some((counters.bytes_in, counters.bytes_out))
}

/// Map a numeric NTSTATUS to a readable name (covering the most common
/// values).
fn nt_status_name(status: u32) -> &'static str {
    match status {
        0 => "STATUS_SUCCESS",
        0xC0000003 => "STATUS_INVALID_INFO_CLASS",
        0xC0000004 => "STATUS_INFO_LENGTH_MISMATCH",
        0xC0000005 => "STATUS_ACCESS_VIOLATION",
        0xC000000D => "STATUS_INVALID_PARAMETER",
        0xC0000022 => "STATUS_ACCESS_DENIED",
        0xC0000225 => "STATUS_NOT_FOUND",
        0xC0000008 => "STATUS_INVALID_HANDLE",
        _ => "UNKNOWN",
    }
}

/// Probe network IO counters for every process and emit human-readable
/// diagnostic text.
#[allow(dead_code)]
pub fn dump_net_io_probe() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "=== Network IO Probe (NtQueryInformationProcess ProcessNetworkIoCounters=114) ==="
    );
    let _ = writeln!(out, "Info class: {} (ProcessNetworkIoCounters)", PROCESS_NETWORK_IO_COUNTERS_CLASS);
    let _ = writeln!(
        out,
        "Struct size: {} bytes (ProcessNetworkCounters {{ u64 BytesIn, u64 BytesOut }})",
        size_of::<ProcessNetworkCounters>()
    );

    let nt_fn_opt = nt_query_information_process();
    let nt_fn = match nt_fn_opt {
        None => {
            let _ = writeln!(
                out,
                "NtQueryInformationProcess: NOT RESOLVED - ntdll!NtQueryInformationProcess not found, network column will always be 0"
            );
            return out;
        }
        Some(f) => {
            let _ = writeln!(out, "NtQueryInformationProcess: resolved OK");
            f
        }
    };

    let entries = match list_basic_entries() {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(out, "list_basic_entries FAILED: {}", e);
            return out;
        }
    };

    let mut total_probed: u32 = 0;
    let mut total_success: u32 = 0;
    let mut total_failed: u32 = 0;
    let mut total_access_denied: u32 = 0;
    let mut total_nonzero: u32 = 0;
    let mut nonzero_lines: Vec<String> = Vec::new();
    let mut failed_lines: Vec<String> = Vec::new();
    let mut first_status_codes: Vec<(u32, u32, String)> = Vec::new(); // (status_code, count, name)

    for entry in &entries {
        let pid = entry.pid;
        if pid == 0 {
            // System Idle Process has no handle, skip it.
            continue;
        }
        total_probed += 1;

        let handle = match open_handle(pid, PROCESS_QUERY_INFORMATION) {
            Some(h) => h,
            None => {
                total_access_denied += 1;
                total_failed += 1;
                if failed_lines.len() < 20 {
                    failed_lines.push(format!(
                        "PID {:>6} ({:<30}) OpenProcess FAILED (access denied)",
                        pid, entry.name
                    ));
                }
                continue;
            }
        };

        let (status_u32, bytes_returned, pnc) = unsafe {
            let mut pnc: ProcessNetworkCounters = std::mem::zeroed();
            let mut bytes_returned: u32 = 0;
            let status: i32 = nt_fn(
                handle,
                PROCESS_NETWORK_IO_COUNTERS_CLASS,
                &mut pnc as *mut _ as *mut std::ffi::c_void,
                size_of::<ProcessNetworkCounters>() as u32,
                &mut bytes_returned,
            );
            (status as u32, bytes_returned, pnc)
        };
        close_handle(handle);

        // Track the first occurrence of each status code (helps understand
        // the failure distribution).
        if !first_status_codes.iter().any(|(s, _, _)| *s == status_u32) {
            first_status_codes.push((status_u32, 0, nt_status_name(status_u32).to_string()));
        }
        let slot = first_status_codes.iter_mut().find(|(s, _, _)| *s == status_u32);
        if let Some((_, count, _)) = slot {
            *count += 1;
        }

        if status_u32 == 0 {
            total_success += 1;
            if pnc.bytes_in > 0 || pnc.bytes_out > 0 {
                total_nonzero += 1;
                if nonzero_lines.len() < 50 {
                    nonzero_lines.push(format!(
                        "PID {:>6} ({:<30}) bytes_returned={:>3} BytesIn={:>14} BytesOut={:>14}",
                        pid, entry.name, bytes_returned, pnc.bytes_in, pnc.bytes_out
                    ));
                }
            }
        } else {
            total_failed += 1;
            if failed_lines.len() < 20 {
                failed_lines.push(format!(
                    "PID {:>6} ({:<30}) status={:#010X} ({}) bytes_returned={}",
                    pid, entry.name, status_u32, nt_status_name(status_u32), bytes_returned
                ));
            }
        }
    }

    let _ = writeln!(out, "");
    let _ = writeln!(out, "=== Summary ===");
    let _ = writeln!(out, "Probed:              {} processes (excl. PID 0 idle)", total_probed);
    let _ = writeln!(out, "STATUS_SUCCESS:      {}", total_success);
    let _ = writeln!(out, "Failed:              {} (of which OpenProcess access denied = {})", total_failed, total_access_denied);
    let _ = writeln!(out, "Non-zero BytesIn/Out: {}", total_nonzero);

    if !first_status_codes.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== NTSTATUS distribution ===");
        // Sort by count descending.
        first_status_codes.sort_by(|a, b| b.1.cmp(&a.1));
        for (status, count, name) in &first_status_codes {
            let _ = writeln!(out, "  {:#010X} ({})  x {}", status, name, count);
        }
    }

    if !nonzero_lines.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== Processes with non-zero network bytes (top 50) ===");
        for line in &nonzero_lines {
            let _ = writeln!(out, "{}", line);
        }
    } else {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== No process reported non-zero network bytes ===");
        let _ = writeln!(out, "Possible causes:");
        let _ = writeln!(out, "  1. The current Windows version does not support ProcessNetworkIoCounters (requires Win11 24H2+ / Build 26100+)");
        let _ = writeln!(out, "  2. Every process's NTSTATUS is a failure (see NTSTATUS distribution and Failed probes above)");
        let _ = writeln!(out, "  3. The system has not generated any network traffic since startup (unlikely; the System process usually has non-zero values)");
    }

    if !failed_lines.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== Failed samples (top 20) ===");
        for line in &failed_lines {
            let _ = writeln!(out, "{}", line);
        }
    }

    out
}
