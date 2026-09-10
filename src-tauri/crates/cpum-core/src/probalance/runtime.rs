//! Service-side runtime: the orchestration layer for the decision engine,
//! the Win32 execution (procwin), and persistence (journal / config).
//!
//! Each [`tick`](ProBalanceRuntime::tick) (1 s) does:
//! hot-reload config (mtime change = reload; no service restart needed
//! when the GUI edits the config) -> sample -> foreground detection ->
//! engine decision -> execute downgrade / restore -> write action log and
//! status file.
//!
//! On construction it performs **startup reconciliation** (see
//! [`Self::reconcile_orphans`]): if the previous run terminated
//! abnormally, the stop-path's `shutdown` restore never ran, and the
//! downgraded processes would stay downgraded forever. Scan the action
//! log for unpaired `downgrade` entries (the original values live in the
//! `from` field) and restore any still-alive process whose name still
//! matches - PID-reuse protection reuses `execute_restore`. This runs
//! regardless of whether the feature is currently enabled (orphan
//! downgrades are a past fact, independent of the current toggle).

use std::path::Path;
use std::time::SystemTime;

use crate::rule::AffinityRule;

use super::config::{load_config, PB_CONFIG_FILE, ProBalanceConfig};
use super::engine::{
    cpu_above_downgrade_target, Decision, ProBalanceEngine, RestoreReason, TickInput,
    DOWNGRADE_IO_PRIORITY, DOWNGRADE_PRIORITY_CLASS,
};
use super::journal::{append_log, pair_orphans, read_log, write_status, PbLogEntry, PbStatus};

/// A foreground priority boost is deliberately tracked independently from
/// ProBalance downgrades so each policy restores only the value it changed.
struct GameModeBoost {
    pid: u32,
    name: String,
    original_priority: u32,
}

const GAME_MODE_PRIORITY_CLASS: u32 = 0x8000; // ABOVE_NORMAL_PRIORITY_CLASS

// =========================================================================
// Runtime
// =========================================================================

/// ProBalance orchestrator for the service main loop.
pub struct ProBalanceRuntime {
    engine: ProBalanceEngine,
    config_mtime: Option<SystemTime>,
    game_boost: Option<GameModeBoost>,
}

impl ProBalanceRuntime {
    pub fn new(base_dir: &Path) -> Self {
        let config = load_config(base_dir).unwrap_or_else(|e| {
            eprintln!("ProBalance config load failed: {e}, using default (disabled)");
            ProBalanceConfig::default()
        });
        let mut rt = Self {
            engine: ProBalanceEngine::new(config),
            config_mtime: config_mtime(base_dir),
            game_boost: None,
        };
        rt.reconcile_orphans(base_dir);
        rt
    }

    /// Startup reconciliation: when the previous run terminated
    /// abnormally (crash / power loss / force-kill), the stop-path's
    /// `shutdown` restore never ran, and downgraded processes will stay
    /// in the downgraded state forever. Scan the action log for unpaired
    /// `downgrade` records (the original values live in the `from`
    /// field) and restore any still-alive process whose name matches -
    /// PID-reuse protection reuses `execute_restore`. Runs regardless of
    /// whether the feature is currently enabled (orphan downgrades are a
    /// past fact, independent of the current toggle).
    fn reconcile_orphans(&mut self, base_dir: &Path) {
        // limit = usize::MAX -> read the entire log (after rotation:
        // .old + current, capped around 1 MB).
        let orphans = pair_orphans(&read_log(base_dir, usize::MAX));
        if orphans.is_empty() {
            return;
        }
        let count = orphans.len();
        let decisions: Vec<Decision> = orphans
            .into_iter()
            .map(|(pid, (name, originals))| Decision::Restore {
                pid,
                name,
                originals,
                reason: RestoreReason::StartupReconcile,
            })
            .collect();
        self.execute(base_dir, decisions);
        eprintln!("ProBalance startup reconciliation: found {count} orphan downgrade(s), attempted restore");
    }

    pub fn engine(&self) -> &ProBalanceEngine {
        &self.engine
    }

    /// Called once per second by the service main loop.
    pub fn tick(
        &mut self,
        base_dir: &Path,
        sampler: &mut crate::monitor::CpuSampler,
        protecting_rules: &[AffinityRule],
    ) {
        // 1. Hot-reload config.
        let mtime = config_mtime(base_dir);
        if mtime != self.config_mtime {
            self.config_mtime = mtime;
            match load_config(base_dir) {
                Ok(cfg) => {
                    let restores = self.engine.update_config(cfg);
                    self.execute(base_dir, restores);
                }
                Err(e) => eprintln!("ProBalance config reload failed: {e}"),
            }
        }

        // 2. Engine disabled -> only write status (proves the service is
        //    alive); no sampling, no decisions.
        if !self.engine.is_enabled() {
            self.restore_game_boost(base_dir, "disabled");
            let _ = write_status(
                base_dir,
                &PbStatus {
                    ts: unix_now(),
                    enabled: false,
                    engaged: false,
                    downgraded: 0,
                    fg_pid: None,
                    fg_cpu_percent: None,
                    game_mode_active: false,
                },
            );
            return;
        }

        // 3. Sample + foreground detection + decision.
        // The full path is only resolved when a Path-type protecting
        // rule exists (one extra query on the same handle, near-zero
        // cost). The common case with no Path rules passes false for
        // zero sampling overhead.
        let include_paths = protecting_rules
            .iter()
            .any(|r| r.match_type == crate::rule::MatchType::Path);
        let samples = sampler.tick(include_paths).unwrap_or_default();
        let foreground = crate::monitor::get_foreground_state();
        let fg_pid = foreground.pid;
        let game_mode_active = self.engine.config().game_mode_enabled && foreground.fullscreen && fg_pid.is_some();
        let now = unix_now();
        let input = TickInput { now_secs: now, fg_pid, samples: &samples, protecting_rules };
        self.reconcile_game_boost(base_dir, game_mode_active, fg_pid);
        let decisions = self.engine.tick_with_game_mode(&input, game_mode_active);

        // 4. Execute.
        self.execute(base_dir, decisions);

        // 5. Write status.
        let fg_cpu_percent =
            fg_pid.and_then(|p| samples.iter().find(|s| s.pid == p)).map(|s| s.cpu_percent);
        let _ = write_status(
            base_dir,
            &PbStatus {
                ts: now,
                enabled: true,
                engaged: self.engine.is_engaged(),
                downgraded: self.engine.tracked_count(),
                fg_pid,
                fg_cpu_percent,
                game_mode_active,
            },
        );
    }

    /// Called before the service stops: restore every downgraded process.
    pub fn shutdown(&mut self, base_dir: &Path) {
        self.restore_game_boost(base_dir, "shutdown");
        let decisions = self.engine.shutdown();
        self.execute(base_dir, decisions);
    }

    fn reconcile_game_boost(&mut self, base_dir: &Path, active: bool, foreground_pid: Option<u32>) {
        let current = self.game_boost.as_ref().map(|boost| boost.pid);
        if current.is_some() && (!active || current != foreground_pid) {
            self.restore_game_boost(base_dir, if active { "foreground_changed" } else { "fullscreen_ended" });
        }
        let Some(pid) = foreground_pid else { return };
        if !active || self.game_boost.is_some() {
            return;
        }
        let priorities = crate::procwin::get_process_priorities(pid);
        let Some(original_priority) = priorities.priority_class else { return };
        // Never lower an already-higher foreground priority and avoid
        // boosting unknown/raw classes where ordering cannot be proven.
        if crate::procwin::priority_class_rank(original_priority)
            .is_none_or(|rank| rank >= crate::procwin::priority_class_rank(GAME_MODE_PRIORITY_CLASS).unwrap())
        {
            return;
        }
        let Some(name) = crate::procwin::get_process_name(pid) else { return };
        if crate::procwin::set_process_priority_class(pid, GAME_MODE_PRIORITY_CLASS).is_err() {
            return;
        }
        self.game_boost = Some(GameModeBoost { pid, name: name.clone(), original_priority });
        let _ = append_log(base_dir, &PbLogEntry {
            ts: unix_now(), action: "game_mode_boost".into(), reason: None, pid, name,
            cpu: None, from: Some(priorities), to: Some(crate::procwin::ProcessPriorities {
                priority_class: Some(GAME_MODE_PRIORITY_CLASS), ..priorities
            }),
        });
    }

    fn restore_game_boost(&mut self, base_dir: &Path, reason: &str) {
        let Some(boost) = self.game_boost.take() else { return };
        let same_process = crate::procwin::get_process_name(boost.pid)
            .is_some_and(|name| crate::matcher::name_matches(&name, &boost.name));
        let restored = same_process
            && crate::procwin::set_process_priority_class(boost.pid, boost.original_priority).is_ok();
        let _ = append_log(base_dir, &PbLogEntry {
            ts: unix_now(), action: "game_mode_restore".into(), reason: Some(reason.into()),
            pid: boost.pid, name: boost.name, cpu: None, from: None,
            to: if restored { Some(crate::procwin::ProcessPriorities { priority_class: Some(boost.original_priority), ..Default::default() }) } else { None },
        });
    }

    /// Execute decisions: downgrade reads original values, writes the
    /// lower values, then calls back to track them; restore writes the
    /// values back; everything is logged.
    fn execute(&mut self, base_dir: &Path, decisions: Vec<Decision>) {
        let now = unix_now();
        for d in decisions {
            match d {
                Decision::Downgrade { pid, name, cpu_percent } => {
                    self.execute_downgrade(base_dir, now, pid, name, cpu_percent);
                }
                Decision::Restore { pid, name, originals, reason } => {
                    self.execute_restore(base_dir, now, pid, name, originals, reason);
                }
            }
        }
    }

    fn execute_downgrade(
        &mut self,
        base_dir: &Path,
        now: u64,
        pid: u32,
        name: String,
        cpu_percent: f32,
    ) {
        let prios = crate::procwin::get_process_priorities(pid);
        // Cannot read the CPU priority (privilege / exited) -> skip, to
        // avoid downgrading without being able to restore.
        let Some(pc) = prios.priority_class else { return };
        let cpu_high = cpu_above_downgrade_target(pc);
        let io_high = prios.io_priority.map_or(false, |io| io > DOWNGRADE_IO_PRIORITY);
        if !cpu_high && !io_high {
            return; // Already low enough; nothing to do.
        }

        let mut applied = false;
        let mut to = prios;
        if cpu_high
            && crate::procwin::set_process_priority_class(pid, DOWNGRADE_PRIORITY_CLASS).is_ok()
        {
            to.priority_class = Some(DOWNGRADE_PRIORITY_CLASS);
            applied = true;
        }
        if io_high
            && crate::procwin::set_process_io_priority(pid, DOWNGRADE_IO_PRIORITY).is_ok()
        {
            to.io_priority = Some(DOWNGRADE_IO_PRIORITY);
            applied = true;
        }

        if applied {
            self.engine.mark_downgraded(pid, name.clone(), prios, now);
            let _ = append_log(
                base_dir,
                &PbLogEntry {
                    ts: now,
                    action: "downgrade".into(),
                    reason: None,
                    pid,
                    name,
                    cpu: Some(cpu_percent),
                    from: Some(prios),
                    to: Some(to),
                },
            );
        }
    }

    fn execute_restore(
        &mut self,
        base_dir: &Path,
        now: u64,
        pid: u32,
        name: String,
        originals: crate::procwin::ProcessPriorities,
        reason: RestoreReason,
    ) {
        // PID-reuse protection: Windows PIDs are recycled aggressively;
        // after a downgraded process exits, the same PID may now belong
        // to an unrelated new process, and writing the original values
        // would corrupt that new process. Before writing back, verify
        // the current process name matches the one we downgraded; on a
        // mismatch (or lookup failure) only log the event and skip the
        // write. ProcessExited never writes back, so skip the check.
        let mut written = false;
        if reason != RestoreReason::ProcessExited {
            let same_process = crate::procwin::get_process_name(pid)
                .is_some_and(|current| crate::matcher::name_matches(&current, &name));
            if same_process {
                if let Some(pc) = originals.priority_class {
                    let _ = crate::procwin::set_process_priority_class(pid, pc);
                }
                if let Some(io) = originals.io_priority {
                    let _ = crate::procwin::set_process_io_priority(pid, io);
                }
                written = true;
            }
        }
        let _ = append_log(
            base_dir,
            &PbLogEntry {
                ts: now,
                action: if reason == RestoreReason::ProcessExited {
                    "exit".into()
                } else {
                    "restore".into()
                },
                reason: Some(reason.as_str().to_string()),
                pid,
                name,
                cpu: None,
                from: None,
                // When the write-back was skipped, set `to` to None so
                // the GUI log can distinguish "restored" from "wanted to
                // restore but did not".
                to: if written || reason == RestoreReason::ProcessExited {
                    Some(originals)
                } else {
                    None
                },
            },
        );
    }
}

// =========================================================================
// Helpers
// =========================================================================

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn config_mtime(base_dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(base_dir.join(PB_CONFIG_FILE))
        .ok()
        .and_then(|m| m.modified().ok())
}
