//! Process enumeration: ToolHelp fast scan + parallel handle walk
//! (affinity / three priority classes / metrics snapshots).
//!
//! Three paths, increasing cost:
//!  - [`list_processes_light`]: pure fast scan, <20ms, populates the first
//!    frame immediately.
//!  - [`list_processes`]: full scan (fast scan + OpenProcess + differential
//!    rates), backfills the first frame in the background.
//!  - [`enumerate_with_snapshots`]: intermediate layer shared with the
//!    metrics stream (does not assemble Vec<ProcessInfo>).

use std::collections::HashMap;
use std::mem::size_of;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, WIN32_ERROR};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};

use crate::models::ProcessInfo;

// Affinity / CPU Sets / three priority classes are all read/written in
// cpum-core (shared single implementation with the service); here we only
// reuse the handle-related read helpers.
use cpum_core::procwin::{
    mask_to_hex, query_image_path_from_handle, read_affinity_with_handle,
    read_priorities_with_handle, ProcessPriorities,
};

use super::sampling::{
    close_handle, get_number_of_processors, open_handle_for_stats, read_metrics_for_handle,
    MIN_SAMPLE_INTERVAL_MS, RATE_CACHE, RateSample, SNAPSHOT_CACHE,
};

// ---------- Enumeration structures ----------

pub(super) struct ProcessBase {
    pub(super) pid: u32,
    pub(super) name: String,
    /// Full executable path (only resolved during full enumeration; None for
    /// the light fast scan or protected processes).
    pub(super) exe_path: Option<String>,
    pub(super) affinity_mask: Option<String>,
    pub(super) system_affinity_mask: Option<String>,
    pub(super) group_affinity_masks: Option<Vec<String>>,
    pub(super) group_system_affinity_masks: Option<Vec<String>>,
    pub(super) parent_pid: u32,
    pub(super) access_denied: bool,
    pub(super) memory_bytes: u64,
    pub(super) priorities: ProcessPriorities,
}

/// Entry returned by the lightweight fast scan: only PID / name / parent_pid,
/// no OpenProcess call (<20ms).
#[derive(Clone, Debug)]
pub(super) struct RawEntry {
    pub(super) pid: u32,
    pub(super) name: String,
    pub(super) parent_pid: u32,
}

// ---------- ToolHelp fast scan ----------

/// Pure ToolHelp SNAPPROCESS fast scan - only PID / name / parent_pid.
/// Performs no OpenProcess / NtQuery calls -> on the order of 10ms.
pub(super) fn list_basic_entries() -> Result<Vec<RawEntry>, String> {
    let pe32_size = size_of::<PROCESSENTRY32W>() as u32;
    let mut raw: Vec<RawEntry> = Vec::with_capacity(512);

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot failed: {}", e))?;

        let mut entry = PROCESSENTRY32W {
            dwSize: pe32_size,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                entry.dwSize = pe32_size;
                raw.push(RawEntry {
                    pid: entry.th32ProcessID,
                    name: pcwstr_to_string(&entry.szExeFile),
                    parent_pid: entry.th32ParentProcessID,
                });

                entry.dwSize = pe32_size;
                match Process32NextW(snapshot, &mut entry) {
                    Ok(()) => continue,
                    Err(e) => {
                        let last_err = WIN32_ERROR::from_error(&e).map(|x| x.0).unwrap_or(0);
                        if last_err == 18 { break; } // ERROR_NO_MORE_FILES
                        let mut next_ok = false;
                        for _ in 0..5 {
                            entry.dwSize = pe32_size;
                            match Process32NextW(snapshot, &mut entry) {
                                Ok(()) => { next_ok = true; break; }
                                Err(e2) => {
                                    let err2 = WIN32_ERROR::from_error(&e2).map(|x| x.0).unwrap_or(0);
                                    if err2 == 18 { break; }
                                }
                            }
                        }
                        if next_ok { continue; }
                        break;
                    }
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }

    if !raw.iter().any(|r| r.pid == 0) {
        raw.insert(
            0,
            RawEntry {
                pid: 0,
                name: "System Idle Process".to_string(),
                parent_pid: 0,
            },
        );
    }

    Ok(raw)
}

// ---------- Parallel full enumeration ----------

pub(super) fn enumerate_with_snapshots(
) -> Result<(Vec<ProcessBase>, HashMap<u32, super::sampling::ProcessSnapshot>), String> {
    let raw = list_basic_entries()?;

    // =====================================================================
    // Phase 2: walk the raw list in parallel, open each process to read
    //          affinity + metrics (fully decoupled from the ToolHelp
    //          snapshot).
    //          ★ Performance-critical ★: 343 processes * 4 syscalls =
    //          ~1.3k OpenProcess calls. Single-threaded takes ~1-2s; with
    //          std::thread::scope and per-chunk batching (no rayon dep),
    //          the syscall-level concurrency cuts this to ~300ms.
    // =====================================================================
    use super::sampling::ProcessSnapshot;

    let nproc = get_number_of_processors() as usize;
    let n_threads = nproc.clamp(2, 8);
    let chunk_size = (raw.len() + n_threads - 1) / n_threads;
    let chunks: Vec<&[RawEntry]> = raw.chunks(chunk_size).collect();

    let mut all_results: Vec<(usize, ProcessBase, ProcessSnapshot)> =
        Vec::with_capacity(raw.len());

    std::thread::scope(|s| {
        let handles: Vec<std::thread::ScopedJoinHandle<'_, Vec<(usize, ProcessBase, ProcessSnapshot)>>> =
            chunks.iter().enumerate().map(|(chunk_idx, chunk)| {
                let chunk_start = chunk_idx * chunk_size;
                s.spawn(move || {
                    let mut out: Vec<(usize, ProcessBase, ProcessSnapshot)> =
                        Vec::with_capacity(chunk.len());
                    for (i, r) in chunk.iter().enumerate() {
                        let global_idx = chunk_start + i;
                        let (affinity_mask, system_affinity_mask, access_denied, exe_path, mem_bytes, priorities, snap) =
                            match open_handle_for_stats(r.pid) {
                                (None, _) => (
                                    None,
                                    None,
                                    true,
                                    None,
                                    0,
                                    ProcessPriorities::default(),
                                    ProcessSnapshot::default(),
                                ),
                                (Some(h), partially_denied) => {
                                    let (pm, sm) = read_affinity_with_handle(h);
                                    let m = read_metrics_for_handle(h);
                                    let prios = read_priorities_with_handle(h);
                                    // Handle already open: resolve the full path here
                                    // (reuse the handle - no extra OpenProcess).
                                    let exe_path = query_image_path_from_handle(h);
                                    close_handle(h);
                                    let snap = ProcessSnapshot {
                                        cpu_total_ticks: m.cpu_total_ticks.unwrap_or(0),
                                        disk_read_bytes: m.disk_read_bytes.unwrap_or(0),
                                        disk_write_bytes: m.disk_write_bytes.unwrap_or(0),
                                        net_in_bytes: m.net_in_bytes.unwrap_or(0),
                                        net_out_bytes: m.net_out_bytes.unwrap_or(0),
                                    };
                                    let denied =
                                        partially_denied || (pm.is_none() && m.cpu_total_ticks.is_none());
                                    (
                                        pm.map(mask_to_hex),
                                        sm.map(mask_to_hex),
                                        denied,
                                        exe_path,
                                        m.working_set_bytes,
                                        prios,
                                        snap,
                                    )
                                }
                            };

                        out.push((
                            global_idx,
                                ProcessBase {
                                pid: r.pid,
                                name: r.name.clone(),
                                exe_path,
                                affinity_mask,
                                    system_affinity_mask,
                                    group_affinity_masks: None,
                                    group_system_affinity_masks: None,
                                parent_pid: r.parent_pid,
                                access_denied,
                                memory_bytes: mem_bytes,
                                priorities,
                            },
                            snap,
                        ));
                    }
                    out
                })
            }).collect();

        for h in handles {
            if let Ok(v) = h.join() {
                all_results.extend(v);
            }
        }
    });

    // Restore the original order (parallel chunk processing may reorder
    // entries).
    all_results.sort_by_key(|(i, _, _)| *i);

    let group_masks = if cpum_core::procwin::active_group_count() > 1 {
        cpum_core::procwin::aggregate_group_affinity_by_pid().ok()
    } else { None };
    let group_system_masks = if cpum_core::procwin::active_group_count() > 1 {
        Some((0..cpum_core::procwin::active_group_count())
            .map(|group| {
                let count = cpum_core::procwin::active_processor_count(group);
                let mask = if count >= 64 { u64::MAX } else { (1u64 << count) - 1 };
                mask_to_hex(mask)
            })
            .collect())
    } else { None };
    let mut bases: Vec<ProcessBase> = Vec::with_capacity(all_results.len());
    let mut snaps: HashMap<u32, ProcessSnapshot> = HashMap::with_capacity(all_results.len());
    for (_, mut base, snap) in all_results {
        base.group_affinity_masks = group_masks.as_ref().and_then(|m| m.get(&base.pid))
            .map(|masks| masks.iter().map(|mask| mask_to_hex(*mask)).collect());
        base.group_system_affinity_masks = group_system_masks.clone();
        snaps.insert(base.pid, snap);
        bases.push(base);
    }

    Ok((bases, snaps))
}

// ---------- Public API: light list / full list ----------

/// Ultra-lightweight process list: only PID / name / parent_pid, no
/// OpenProcess at all. Returns in <20ms, used to populate the first frame
/// immediately; slow fields (memory / affinity / rates) are patched in
/// later.
pub fn list_processes_light() -> Result<Vec<ProcessInfo>, String> {
    let raw = list_basic_entries()?;
    let mut out: Vec<ProcessInfo> = Vec::with_capacity(raw.len());
    for r in raw {
        out.push(ProcessInfo {
            pid: r.pid,
            name: r.name,
            affinity_mask: None,
            system_affinity_mask: None,
            group_affinity_masks: None,
            group_system_affinity_masks: None,
            parent_pid: r.parent_pid,
            access_denied: false, // Unknown: assume we have access; the full
                                  // enumeration patch will overwrite.
            cpu_usage_percent: 0.0,
            memory_bytes: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_in_bps: 0,
            net_out_bps: 0,
        });
    }
    out.sort_by(|a, b| system_process_first(a.pid, b.pid));
    Ok(out)
}

pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let now = Instant::now();
    let nproc = get_number_of_processors() as f32;

    // 1. Fetch the previous cache (may be None = first call).
    let prev_opt: Option<(Instant, HashMap<u32, super::sampling::ProcessSnapshot>)> = {
        let guard = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        guard.clone()
    };
    let (prev_time, prev_map) = prev_opt
        .clone()
        .unwrap_or_else(|| (now, HashMap::new()));
    let dt_ms = now.saturating_duration_since(prev_time).as_millis();

    // 2. Enumerate all processes + collect the current snapshot.
    let (processes_base, snap_map) = enumerate_with_snapshots()?;

    // 3. Whenever the interval is >= threshold, compute rates once. (On the
    // first call prev_map is empty, so all rates remain 0 - this is
    // expected; it's used as a baseline anchor.)
    let mut rate_guard = RATE_CACHE.lock().map_err(|e| e.to_string())?;
    if dt_ms >= MIN_SAMPLE_INTERVAL_MS {
        let dt_sec = (dt_ms as f64) / 1000.0;
        for (pid, snap) in snap_map.iter() {
            let prev = prev_map.get(pid).copied();

            // Disk delta
            let (disk_rb, disk_wb) = match prev {
                Some(p) => (
                    snap.disk_read_bytes.saturating_sub(p.disk_read_bytes),
                    snap.disk_write_bytes.saturating_sub(p.disk_write_bytes),
                ),
                None => (0, 0),
            };

            // Net delta (BytesIn / BytesOut, Win11 24H2+; older versions
            // keep the snap fields at 0).
            let (net_in_d, net_out_d) = match prev {
                Some(p) => (
                    snap.net_in_bytes.saturating_sub(p.net_in_bytes),
                    snap.net_out_bytes.saturating_sub(p.net_out_bytes),
                ),
                None => (0, 0),
            };

            // CPU %
            let cpu_percent = match prev {
                Some(p) => {
                    let delta = snap.cpu_total_ticks.saturating_sub(p.cpu_total_ticks) as f64;
                    let delta_cpu_sec = delta * 1e-7;
                    ((delta_cpu_sec / dt_sec) * 100.0) as f32
                }
                None => 0.0,
            };
            let cpu_percent = cpu_percent.max(0.0).min(nproc * 100.0 * 1.1);

            rate_guard.insert(
                *pid,
                RateSample {
                    cpu_percent,
                    disk_read_bps: (disk_rb as f64 / dt_sec) as u64,
                    disk_write_bps: (disk_wb as f64 / dt_sec) as u64,
                    net_in_bps: (net_in_d as f64 / dt_sec) as u64,
                    net_out_bps: (net_out_d as f64 / dt_sec) as u64,
                },
            );
        }
        rate_guard.retain(|pid, _| snap_map.contains_key(pid));
    } else if prev_opt.is_none() {
        // First call: prune dead PIDs (only keep currently-existing
        // processes).
        rate_guard.retain(|pid, _| snap_map.contains_key(pid));
    }

    // 4. ★Key point★: regardless of whether we triggered rate computation,
    //    write the current snapshot to the cache so the next call has a
    //    baseline to diff against. (Earlier bug: the cache was only written
    //    inside the `if` branch, so the first call never wrote anything and
    //    every subsequent dt_ms stayed at 0.)
    {
        let mut cache = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((now, snap_map));
    }

    // Assemble the final result.
    let mut out: Vec<ProcessInfo> = Vec::with_capacity(processes_base.len());
    for pb in processes_base {
        let rs = rate_guard.get(&pb.pid).copied().unwrap_or_default();
        out.push(ProcessInfo {
            pid: pb.pid,
            name: pb.name,
            affinity_mask: pb.affinity_mask,
            system_affinity_mask: pb.system_affinity_mask,
            group_affinity_masks: pb.group_affinity_masks,
            group_system_affinity_masks: pb.group_system_affinity_masks,
            parent_pid: pb.parent_pid,
            access_denied: pb.access_denied,
            cpu_usage_percent: rs.cpu_percent,
            memory_bytes: pb.memory_bytes,
            disk_read_bps: rs.disk_read_bps,
            disk_write_bps: rs.disk_write_bps,
            net_in_bps: rs.net_in_bps,
            net_out_bps: rs.net_out_bps,
        });
    }

    out.sort_by(|a, b| system_process_first(a.pid, b.pid));
    Ok(out)
}

// ---------- Process list cache (faster first frame: the next launch shows
// last session's process list directly, slow fields are 0) ----------

fn processes_cache_path(base_dir: &std::path::Path) -> std::path::PathBuf {
    base_dir.join("processes_cache.json")
}

/// Read the process list cache saved from the previous run. Contains only
/// PID/name/parent_pid with slow fields at 0. Returns None if no cache
/// exists or parsing failed; the caller should fall back to
/// `list_processes_light`.
pub fn load_processes_cache(base_dir: &std::path::Path) -> Option<Vec<ProcessInfo>> {
    let path = processes_cache_path(base_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<Vec<ProcessInfo>>(&raw).ok()
}

/// Write the process list to the cache (basic fields only; slow fields are
/// also stored so they display directly next launch). Failures are silently
/// ignored (the cache is just an optimization).
pub fn save_processes_cache(base_dir: &std::path::Path, processes: &[ProcessInfo]) {
    let _ = std::fs::create_dir_all(base_dir);
    let path = processes_cache_path(base_dir);
    if let Ok(json) = serde_json::to_string(processes) {
        let _ = std::fs::write(&path, json);
    }
}

// ---------- Internal helpers ----------

fn pcwstr_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn is_system_process(pid: u32) -> bool {
    pid == 0 || pid == 4
}

/// Stable sort: system processes (PID 0/4) first, the rest sorted by PID
/// ascending (a less surprising initial ordering for the user).
fn system_process_first(a: u32, b: u32) -> std::cmp::Ordering {
    match (is_system_process(a), is_system_process(b)) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.cmp(&b),
    }
}
