//! Action log and status persistence (journal: for the GUI to read and for
//! the service to reconcile on restart).
//!
//! - **Action log** `probalance_log.jsonl`: one JSON per downgrade/restore,
//!   rotated to `.old` (one generation kept) once the file exceeds 512 KB.
//!   JSONL append + skip-malformed-line parsing: a service crash never
//!   loses already-persisted entries.
//! - **Status file** `probalance_status.json`: an instantaneous snapshot
//!   overwritten every tick. The GUI uses the timestamp freshness to
//!   decide whether the service is alive (atomic write prevents torn
//!   reads).
//!
//! [`pair_orphans`] derives unpaired orphan downgrades from the log - the
//! basis for the service's startup reconciliation.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::procwin::ProcessPriorities;

use super::config::atomic_write;

// =========================================================================
// Action log (JSONL + simple rotation)
// =========================================================================

/// Log file name (under `base_dir`).
pub const PB_LOG_FILE: &str = "probalance_log.jsonl";
/// Log rotation threshold (rename to `.old` when exceeded; one generation
/// kept).
pub const PB_LOG_ROTATE_BYTES: u64 = 512 * 1024;

/// One action log entry (displayed in the GUI panel).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PbLogEntry {
    /// Unix seconds.
    pub ts: u64,
    /// "downgrade" | "restore" | "exit".
    pub action: String,
    /// Restore reason (for restore/exit):
    /// contention_cleared / timeout / process_exited / disabled /
    /// shutdown.
    pub reason: Option<String>,
    pub pid: u32,
    pub name: String,
    /// CPU usage at the time of the downgrade (single-core baseline %).
    pub cpu: Option<f32>,
    /// Priorities before the action.
    pub from: Option<ProcessPriorities>,
    /// Priorities after the action.
    pub to: Option<ProcessPriorities>,
}

/// Durable aggregate derived from the retained action journal. This avoids a
/// second mutable statistics store and remains correct after service restarts.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PbStatistics {
    pub downgrade_count: u64,
    pub restore_count: u64,
    pub exit_count: u64,
    pub unique_processes: u64,
}

pub fn statistics(base_dir: &Path) -> PbStatistics {
    let entries = read_log(base_dir, usize::MAX);
    let mut result = PbStatistics::default();
    let mut processes = std::collections::HashSet::new();
    for entry in entries {
        processes.insert((entry.pid, entry.name));
        match entry.action.as_str() {
            "downgrade" => result.downgrade_count += 1,
            "restore" => result.restore_count += 1,
            "exit" => result.exit_count += 1,
            _ => {}
        }
    }
    result.unique_processes = processes.len() as u64;
    result
}

/// Append one log entry (auto-rotate when over the limit; a single failure
/// does not interrupt the service).
pub fn append_log(base_dir: &Path, entry: &PbLogEntry) -> Result<(), String> {
    let path = base_dir.join(PB_LOG_FILE);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > PB_LOG_ROTATE_BYTES {
            let _ = std::fs::rename(&path, base_dir.join(format!("{PB_LOG_FILE}.old")));
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut line = serde_json::to_string(entry).map_err(|e| format!("failed to serialize log: {e}"))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("failed to open log file: {e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("failed to write log: {e}"))
}

/// Read the most recent `limit` log entries (merges `.old` and the current
/// file; malformed lines are skipped).
///
/// Read both files before taking the tail - if `.old` alone hits `limit`,
/// we stop early and silently miss the newest entries in the current file
/// (after one rotation the GUI would forever show stale data). After
/// finishing each file, trim the intermediate buffer to `limit` entries
/// to defend against rotation failures producing huge files.
pub fn read_log(base_dir: &Path, limit: usize) -> Vec<PbLogEntry> {
    let mut all: Vec<PbLogEntry> = Vec::new();
    for file_name in [format!("{PB_LOG_FILE}.old"), PB_LOG_FILE.to_string()] {
        let path = base_dir.join(file_name);
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for line in raw.lines() {
                if let Ok(entry) = serde_json::from_str::<PbLogEntry>(line) {
                    all.push(entry);
                }
            }
        }
        // For `.old` alone, keep only the last `limit` entries: the
        // current file's new entries can only push older ones out, so
        // keeping too many would be pointless.
        if all.len() > limit {
            all.drain(..all.len() - limit);
        }
    }
    all
}

/// Compute unpaired orphan downgrades from the action log entries
/// (pid -> name + pre-downgrade original values).
///
/// Scan in time order: a `downgrade` inserts/overwrites, `restore`|`exit`
/// removes - so multiple "downgrade -> restore" rounds on the same PID
/// only register as an orphan if the last round is unpaired. Used by the
/// service startup reconciliation.
pub(super) fn pair_orphans(entries: &[PbLogEntry]) -> HashMap<u32, (String, ProcessPriorities)> {
    let mut orphans: HashMap<u32, (String, ProcessPriorities)> = HashMap::new();
    for e in entries {
        match e.action.as_str() {
            "downgrade" => {
                if let Some(from) = e.from {
                    orphans.insert(e.pid, (e.name.clone(), from));
                }
            }
            "restore" | "exit" => {
                orphans.remove(&e.pid);
            }
            _ => {}
        }
    }
    orphans
}

// =========================================================================
// Status file (GUI polls this to see the service-side engine state)
// =========================================================================

pub const PB_STATUS_FILE: &str = "probalance_status.json";

/// Service-side engine instantaneous state (overwritten every tick).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PbStatus {
    /// Write time (Unix seconds) - GUI uses the freshness to decide
    /// whether the service is alive.
    pub ts: u64,
    pub enabled: bool,
    pub engaged: bool,
    pub downgraded: usize,
    pub fg_pid: Option<u32>,
    pub fg_cpu_percent: Option<f32>,
    #[serde(default)]
    pub game_mode_active: bool,
}

pub fn write_status(base_dir: &Path, status: &PbStatus) -> Result<(), String> {
    let path = base_dir.join(PB_STATUS_FILE);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::to_string(status).map_err(|e| format!("failed to serialize status: {e}"))?;
    atomic_write(&path, &json)
}

/// Read the status file (None when the file does not exist - the service
/// has never run).
pub fn read_status(base_dir: &Path) -> Option<PbStatus> {
    let path = base_dir.join(PB_STATUS_FILE);
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}
