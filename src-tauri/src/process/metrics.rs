//! Staged metrics push: one sample per second -> 4 wave events (CPU /
//! memory / disk / network) + 1 structural diff event.
//!
//! Replaces the old "poll everything every second + re-render the whole
//! table" approach. Each column on the frontend updates independently, so
//! the entire table never flickers. Sampling and the diff baseline reuse the
//! global cache in [`super::sampling`], sharing the same snapshot with
//! `list_processes` (whichever runs first establishes the baseline; they
//! don't interfere with each other).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;

use once_cell::sync::Lazy;
use serde::Serialize;

use cpum_core::procwin::ProcessPriorities;

use super::enumerate::enumerate_with_snapshots;
use super::sampling::{
    get_number_of_processors, RateSample, MIN_SAMPLE_INTERVAL_MS, RATE_CACHE, SNAPSHOT_CACHE,
};

// =========================================================================
// Event model
// =========================================================================

/// Single-wave metrics event payload (compact tuple encoding, as small as
/// possible).
/// wave 1: each batch entry = [pid, cpu_percent]
/// wave 2: each batch entry = [pid, memory_bytes]
/// wave 3: each batch entry = [pid, disk_read_bps, disk_write_bps]
/// wave 4: each batch entry = [pid, net_in_bps, net_out_bps]
#[derive(Serialize, Clone, Debug)]
pub struct MetricsWaveEvent {
    pub wave: u8,
    /// number[][] - skip introducing extra dependent types; use a
    /// serde_json-friendly nested array directly.
    pub batch: Vec<Vec<serde_json::Value>>,
}

/// Structural change event (PID set added/exited, or slow-changing fields
/// such as name/affinity/ppid have been updated).
#[derive(Serialize, Clone, Debug)]
pub struct ProcessDiffEvent {
    /// Structural field snapshot (no rate fields), for newly added or
    /// potentially-updated processes.
    pub upserts: Vec<ProcessBaseSnapshot>,
    /// PIDs that exited this round and should be spliced out on the frontend.
    pub removed_pids: Vec<u32>,
}

/// `ProcessBase` snapshot without rate fields (used by the frontend's
/// `applyProcessDiff`).
#[derive(Serialize, Clone, Debug)]
pub struct ProcessBaseSnapshot {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub affinity_mask: Option<String>,
    pub system_affinity_mask: Option<String>,
    pub group_affinity_masks: Option<Vec<String>>,
    pub parent_pid: u32,
    pub access_denied: bool,
    pub memory_bytes: u64,
    pub priority_class: Option<u32>,
    pub io_priority: Option<u32>,
    pub memory_priority: Option<u32>,
}

/// Output of a full sample: rates + structural info + diff. The caller
/// distributes the data across the 4 emit events.
pub struct MetricsTick {
    pub rates: HashMap<u32, RateSample>,
    pub memory_by_pid: HashMap<u32, u64>,
    pub removed_pids: Vec<u32>,
    /// New or structurally-changed processes (with a structural snapshot).
    pub upserts: Vec<ProcessBaseSnapshot>,
}

// =========================================================================
// Collection
// =========================================================================

/// Extracts the "collect + diff calculation + write baseline cache" core of
/// `list_processes` and returns a structured `MetricsTick` without doing the
/// (large) `Vec<ProcessInfo>` assembly - that step is only needed for the
/// first full-frame load.
pub fn collect_metrics_tick() -> Result<MetricsTick, String> {
    let now = Instant::now();
    let nproc = get_number_of_processors() as f32;

    let prev_opt: Option<(Instant, HashMap<u32, super::sampling::ProcessSnapshot>)> = {
        let guard = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        guard.clone()
    };
    let (prev_time, prev_map) = prev_opt
        .clone()
        .unwrap_or_else(|| (now, HashMap::new()));
    let dt_ms = now.saturating_duration_since(prev_time).as_millis();

    let (processes_base, snap_map) = enumerate_with_snapshots()?;

    // The alive pid set for this round (used to detect removed processes).
    let alive_set: std::collections::HashSet<u32> = snap_map.keys().copied().collect();

    // ---- removed ----
    let removed_pids: Vec<u32> = prev_opt
        .as_ref()
        .map(|(_, pm)| pm.keys().copied().filter(|p| !alive_set.contains(p)).collect())
        .unwrap_or_default();

    // ---- rates ----
    let mut rate_guard = RATE_CACHE.lock().map_err(|e| e.to_string())?;
    if dt_ms >= MIN_SAMPLE_INTERVAL_MS {
        let dt_sec = (dt_ms as f64) / 1000.0;
        for (pid, snap) in snap_map.iter() {
            let prev = prev_map.get(pid).copied();
            let (disk_rb, disk_wb) = match prev {
                Some(p) => (
                    snap.disk_read_bytes.saturating_sub(p.disk_read_bytes),
                    snap.disk_write_bytes.saturating_sub(p.disk_write_bytes),
                ),
                None => (0, 0),
            };
            let (net_in_d, net_out_d) = match prev {
                Some(p) => (
                    snap.net_in_bytes.saturating_sub(p.net_in_bytes),
                    snap.net_out_bytes.saturating_sub(p.net_out_bytes),
                ),
                None => (0, 0),
            };
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
        rate_guard.retain(|pid, _| alive_set.contains(pid));
    } else if prev_opt.is_none() {
        rate_guard.retain(|pid, _| alive_set.contains(pid));
    }
    // Copy the rates then drop the lock right away.
    let rates: HashMap<u32, RateSample> = rate_guard.clone();
    drop(rate_guard);

    // Write the baseline cache (key step - keep behavior consistent with
    // `list_processes`).
    {
        let mut cache = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((now, snap_map));
    }

    // ---- memory & upserts ----
    // To avoid pushing the full structure JSON for all 343 processes every
    // round, only push `upserts` for:
    //  a) new PIDs (not in prev_map)
    //  b) significant changes in the memory Working Set (4 KB threshold to
    //     filter out noise; users also want memory to update incrementally)
    //  c) priority changes (the user may change priorities in the editor /
    //     an external tool; the frontend must follow along)
    //  d) every round, also attach the latest name/ppid/affinity values
    static PREV_STRUCT_MEMORY: Lazy<Mutex<HashMap<u32, u64>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    static PREV_STRUCT_PRIORITY: Lazy<Mutex<HashMap<u32, ProcessPriorities>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    let mut prev_mem = PREV_STRUCT_MEMORY.lock().map_err(|e| e.to_string())?;
    let mut prev_prio = PREV_STRUCT_PRIORITY.lock().map_err(|e| e.to_string())?;

    let mut memory_by_pid = HashMap::with_capacity(processes_base.len());
    let mut upserts = Vec::new();
    for pb in processes_base.iter() {
        memory_by_pid.insert(pb.pid, pb.memory_bytes);
        let is_new = !prev_map.contains_key(&pb.pid);
        let prev_mem_bytes = *prev_mem.get(&pb.pid).unwrap_or(&0);
        let mem_changed = pb.memory_bytes.abs_diff(prev_mem_bytes) > 4 * 1024;
        let prio_changed = prev_prio.get(&pb.pid).copied() != Some(pb.priorities);
        if is_new || mem_changed || prio_changed {
            upserts.push(ProcessBaseSnapshot {
                pid: pb.pid,
                name: pb.name.clone(),
                exe_path: pb.exe_path.clone(),
                affinity_mask: pb.affinity_mask.clone(),
                system_affinity_mask: pb.system_affinity_mask.clone(),
                group_affinity_masks: pb.group_affinity_masks.clone(),
                parent_pid: pb.parent_pid,
                access_denied: pb.access_denied,
                memory_bytes: pb.memory_bytes,
                priority_class: pb.priorities.priority_class,
                io_priority: pb.priorities.io_priority,
                memory_priority: pb.priorities.memory_priority,
            });
            prev_mem.insert(pb.pid, pb.memory_bytes);
            prev_prio.insert(pb.pid, pb.priorities);
        }
    }
    // Removed pids: clean up prev_struct_memory / prev_struct_priority at
    // the same time.
    for pid in removed_pids.iter() {
        prev_mem.remove(pid);
        prev_prio.remove(pid);
    }
    drop(prev_mem);
    drop(prev_prio);

    Ok(MetricsTick {
        rates,
        memory_by_pid,
        removed_pids,
        upserts,
    })
}

// =========================================================================
// Event encoding
// =========================================================================

/// Encode a `MetricsTick` into 4 wave push events + 1 diff event. This lets
/// the frontend update "CPU -> memory -> disk -> network" in sequence,
/// avoiding whole-table flicker.
pub fn build_wave_events(tick: &MetricsTick) -> (Vec<MetricsWaveEvent>, ProcessDiffEvent) {
    // wave 1: pid + cpu_percent
    let wave1: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(
                    (rs.cpu_percent * 100.0).round() as i64 as f64 / 100.0,
                ),
            ]
        })
        .collect();

    // wave 2: pid + memory_bytes
    let wave2: Vec<Vec<serde_json::Value>> = tick
        .memory_by_pid
        .iter()
        .map(|(pid, mem)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(*mem),
            ]
        })
        .collect();

    // wave 3: pid + disk_read_bps + disk_write_bps
    let wave3: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(rs.disk_read_bps),
                serde_json::Value::from(rs.disk_write_bps),
            ]
        })
        .collect();

    // wave 4: pid + net_in_bps + net_out_bps
    let wave4: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(rs.net_in_bps),
                serde_json::Value::from(rs.net_out_bps),
            ]
        })
        .collect();

    let waves = vec![
        MetricsWaveEvent { wave: 1, batch: wave1 },
        MetricsWaveEvent { wave: 2, batch: wave2 },
        MetricsWaveEvent { wave: 3, batch: wave3 },
        MetricsWaveEvent { wave: 4, batch: wave4 },
    ];

    let diff = ProcessDiffEvent {
        upserts: tick.upserts.clone(),
        removed_pids: tick.removed_pids.clone(),
    };

    (waves, diff)
}

// =========================================================================
// Background push thread control
// =========================================================================

static STREAM_RUNNING: AtomicBool = AtomicBool::new(false);
static STREAM_JOIN_HANDLE: Lazy<Mutex<Option<JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn start_metrics_stream_in_thread<EmitFn>(
    interval_ms: u32,
    emit: EmitFn,
) -> Result<(), String>
where
    EmitFn: Fn(&str, serde_json::Value) -> Result<(), String> + Send + 'static,
{
    if STREAM_RUNNING.load(Relaxed) {
        return Ok(());
    }
    STREAM_RUNNING.store(true, Relaxed);

    let interval = std::time::Duration::from_millis(interval_ms.max(100) as u64);

    let handle = std::thread::Builder::new()
        .name("cpum-metrics-stream".into())
        .spawn(move || {
            // ★Delay the first round by 1 second★: avoid the initial
            //  scan of 343 processes happening concurrently with the
            //  first-frame `list_processes` call, which would flood the
            //  system with OpenProcess syscalls and freeze it.
            let wave_gap = std::time::Duration::from_millis(5);
            let first_sleep = std::time::Duration::from_secs(1);
            let mut slept = std::time::Duration::ZERO;
            while slept < first_sleep && STREAM_RUNNING.load(Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                slept += std::time::Duration::from_millis(50);
            }
            loop {
                if !STREAM_RUNNING.load(Relaxed) {
                    break;
                }
                let round_start = Instant::now();

                match collect_metrics_tick() {
                    Ok(tick) => {
                        let (waves, diff) = build_wave_events(&tick);

                        for (i, w) in waves.iter().enumerate() {
                            if !STREAM_RUNNING.load(Relaxed) {
                                break;
                            }
                            let payload = match serde_json::to_value(w) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let _ = emit("process://metrics", payload);
                            // Insert a 5 ms gap between wave 1->2, 2->3,
                            // 3->4 to give the frontend 1 frame to render.
                            if i < waves.len() - 1 {
                                std::thread::sleep(wave_gap);
                            }
                        }

                        // Diff last: processes that were added/removed
                        // need one round of metrics first, then sync.
                        if STREAM_RUNNING.load(Relaxed) {
                            if let Ok(payload) = serde_json::to_value(&diff) {
                                let _ = emit("process://processes-diff", payload);
                            }
                        }
                    }
                    Err(_) => {
                        // Sampling failure: skip this round, do not crash
                        // the thread.
                    }
                }

                if !STREAM_RUNNING.load(Relaxed) {
                    break;
                }
                // Ensure the interval is roughly interval_ms (sleep less
                // if the collection itself took a while).
                let elapsed = round_start.elapsed();
                if let Some(remaining) = interval.checked_sub(elapsed) {
                    // Split into smaller sleeps so a stop signal is acted
                    // on more quickly.
                    let step = std::time::Duration::from_millis(50);
                    let mut slept = std::time::Duration::ZERO;
                    while slept < remaining && STREAM_RUNNING.load(Relaxed) {
                        std::thread::sleep(step.min(remaining - slept));
                        slept = slept.saturating_add(step);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let mut guard = STREAM_JOIN_HANDLE.lock().map_err(|e| e.to_string())?;
    if let Some(old) = guard.replace(handle) {
        drop(old);
    }
    Ok(())
}

pub fn stop_metrics_stream_in_thread() -> Result<(), String> {
    STREAM_RUNNING.store(false, Relaxed);
    if let Some(handle) = STREAM_JOIN_HANDLE
        .lock()
        .map_err(|e| e.to_string())?
        .take()
    {
        // Wait at most 400 ms (the thread may be in a 50 ms fine-grained
        // sleep).
        let _ = std::thread::spawn(move || {
            let _ = handle.join();
        });
    }
    Ok(())
}
