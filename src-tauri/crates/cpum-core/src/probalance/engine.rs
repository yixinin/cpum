//! Decision core: ProBalance pure state machine (never touches Win32; can
//! be fully unit-tested).
//!
//! State machine: idle -> (contention sustained for `sustain_secs`) ->
//! downgrading -> (contention cleared for `restore_after_secs`) -> idle.
//!
//! Input: [`TickInput`] (sample snapshot + foreground PID + protecting
//! rules). Output: a list of [`Decision`]. After a downgrade is applied
//! successfully, the caller invokes
//! [`ProBalanceEngine::mark_downgraded`] to record the original values; a
//! subsequent restore decision carries those values back. The engine
//! itself never reads or writes any process.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::matcher::rule_matches;
use crate::monitor::ProcSample;
use crate::procwin::ProcessPriorities;
use crate::rule::AffinityRule;

// =========================================================================
// Constants
// =========================================================================

/// Downgrade target: CPU priority class -> Below Normal (0x4000).
pub const DOWNGRADE_PRIORITY_CLASS: u32 = 0x4000;
/// Downgrade target: IO priority -> Very Low (0).
pub const DOWNGRADE_IO_PRIORITY: u32 = 0;

/// Determine whether a CPU priority class is **above** the downgrade
/// target (by scheduling rank, not raw numeric value).
///
/// Priority class constants are bit flags; numeric comparison produces
/// systematically wrong answers: NORMAL (0x20) / HIGH (0x80) / REALTIME
/// (0x100) are all numerically smaller than BELOW_NORMAL (0x4000), so a
/// raw comparison would miss 99% of normal background processes. Unknown
/// priority classes (e.g. EcoQoS background flags) are conservatively
/// treated as not needing a downgrade.
pub(super) fn cpu_above_downgrade_target(class: u32) -> bool {
    match (
        crate::procwin::priority_class_rank(class),
        crate::procwin::priority_class_rank(DOWNGRADE_PRIORITY_CLASS),
    ) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Hardcoded allowlist of system-critical processes (merged with the
/// user allowlist; case-insensitive, `.exe` suffix tolerated).
pub const SYSTEM_WHITELIST: &[&str] = &[
    "csrss", "wininit", "winlogon", "services", "lsass", "smss", "svchost", "dwm", "audiodg",
    "logonui", "fontdrvhost", "conhost", "sihost", "taskhostw", "explorer", "system",
    "registry", "memory compression", "cpum", "cpum_service",
];

// =========================================================================
// Decision model
// =========================================================================

/// Restore reason (decides whether the service needs to write back the
/// priorities and what to show in the GUI log).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreReason {
    /// Contention cleared and sustained long enough -> all processes
    /// restored.
    ContentionCleared,
    /// A single process's downgrade timed out.
    Timeout,
    /// Process has exited (no need to write back, only log it).
    ProcessExited,
    /// Configuration was disabled.
    Disabled,
    /// Service is stopping.
    Shutdown,
    /// Service-startup reconciliation: the previous run terminated
    /// abnormally; restore any orphan downgrades from the log.
    StartupReconcile,
}

impl RestoreReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            RestoreReason::ContentionCleared => "contention_cleared",
            RestoreReason::Timeout => "timeout",
            RestoreReason::ProcessExited => "process_exited",
            RestoreReason::Disabled => "disabled",
            RestoreReason::Shutdown => "shutdown",
            RestoreReason::StartupReconcile => "startup_reconcile",
        }
    }
}

/// One action emitted by the engine.
#[derive(Debug)]
pub enum Decision {
    /// Downgrade a background process (the service must call back
    /// [`ProBalanceEngine::mark_downgraded`] after applying it).
    Downgrade {
        pid: u32,
        name: String,
        /// CPU usage at the time of the decision (single-core baseline %,
        /// used in the log).
        cpu_percent: f32,
    },
    /// Restore a process (carries the original values; once emitted, the
    /// process is removed from the tracking table).
    Restore {
        pid: u32,
        name: String,
        originals: ProcessPriorities,
        reason: RestoreReason,
    },
}

/// Engine per-tick input (entirely filled by the caller; this shape is
/// what makes the engine unit-testable).
pub struct TickInput<'a> {
    pub now_secs: u64,
    pub fg_pid: Option<u32>,
    pub samples: &'a [ProcSample],
    /// Enabled rules that manage priorities. The engine never downgrades
    /// processes matched by these rules, to prevent the rule engine's
    /// 5-second priority reset from fighting with ProBalance's downgrade.
    pub protecting_rules: &'a [AffinityRule],
}

/// Tracking record for a downgraded process (used for restore).
struct Tracked {
    name: String,
    originals: ProcessPriorities,
    since_secs: u64,
}

// =========================================================================
// Decision engine
// =========================================================================

/// ProBalance decision engine.
///
/// State: idle -> (contention sustained for `sustain`) -> downgrading ->
/// (contention cleared for `restore_after`) -> idle.
pub struct ProBalanceEngine {
    config: crate::probalance::ProBalanceConfig,
    engaged: bool,
    /// Number of consecutive contention ticks.
    contention_ticks: u32,
    /// Number of consecutive no-contention ticks (hysteresis counter while
    /// `engaged`).
    clear_ticks: u32,
    /// Downgraded processes: pid -> original values.
    tracked: HashMap<u32, Tracked>,
}

impl ProBalanceEngine {
    pub fn new(config: crate::probalance::ProBalanceConfig) -> Self {
        Self {
            config,
            engaged: false,
            contention_ticks: 0,
            clear_ticks: 0,
            tracked: HashMap::new(),
        }
    }

    pub fn config(&self) -> &crate::probalance::ProBalanceConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn is_engaged(&self) -> bool {
        self.engaged
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    fn reset_counters(&mut self) {
        self.engaged = false;
        self.contention_ticks = 0;
        self.clear_ticks = 0;
    }

    /// Take all tracked processes out as restore decisions (drains the
    /// tracking table).
    fn take_all(&mut self, reason: RestoreReason) -> Vec<Decision> {
        self.tracked
            .drain()
            .map(|(pid, t)| Decision::Restore {
                pid,
                name: t.name,
                originals: t.originals,
                reason,
            })
            .collect()
    }

    /// Called by the service after a downgrade is successfully applied:
    /// records the original values used for the eventual restore.
    pub fn mark_downgraded(
        &mut self,
        pid: u32,
        name: String,
        originals: ProcessPriorities,
        now_secs: u64,
    ) {
        self.tracked.insert(pid, Tracked { name, originals, since_secs: now_secs });
    }

    /// Hot-update the configuration. When the switch flips from on to off,
    /// returns a full set of restore decisions; when it flips from off to
    /// on, state is cleared.
    pub fn update_config(
        &mut self,
        config: crate::probalance::ProBalanceConfig,
    ) -> Vec<Decision> {
        let was_enabled = self.config.enabled;
        self.config = config;
        match (was_enabled, self.config.enabled) {
            (true, false) => {
                self.reset_counters();
                self.take_all(RestoreReason::Disabled)
            }
            (false, true) => {
                self.reset_counters();
                vec![]
            }
            _ => vec![],
        }
    }

    /// Service stop: restore every downgraded process.
    pub fn shutdown(&mut self) -> Vec<Decision> {
        self.reset_counters();
        self.take_all(RestoreReason::Shutdown)
    }

    /// Main decision loop (once per second).
    ///
    /// - Foreground not in the sample list (protected / unreadable) ->
    ///   treat as no contention, gracefully degrades.
    /// - Downgrade candidates: CPU above the threshold AND not the
    ///   foreground AND not on the allowlist AND not rule-managed AND
    ///   not already tracked.
    /// - Restore triggers: contention-cleared hysteresis / per-process
    ///   timeout / process exit.
    pub fn tick(&mut self, input: &TickInput) -> Vec<Decision> {
        self.tick_with_game_mode(input, false)
    }

    /// Run one decision tick. Game Mode reuses the established ProBalance
    /// safeguards and candidate filtering, but treats a verified fullscreen
    /// foreground process as contention without requiring a CPU threshold.
    pub fn tick_with_game_mode(&mut self, input: &TickInput, game_mode_active: bool) -> Vec<Decision> {
        let cfg = &self.config;
        if !cfg.enabled {
            return vec![];
        }

        // 1. Contention determination.
        let fg_cpu = input
            .fg_pid
            .and_then(|pid| input.samples.iter().find(|s| s.pid == pid))
            .map(|s| s.cpu_percent);
        let contention = game_mode_active && input.fg_pid.is_some()
            || fg_cpu.map(|c| c >= cfg.fg_cpu_threshold).unwrap_or(false);

        if contention {
            self.contention_ticks = self.contention_ticks.saturating_add(1);
            self.clear_ticks = 0;
        } else {
            self.contention_ticks = 0;
            self.clear_ticks = self.clear_ticks.saturating_add(1);
        }

        // 2. Enter the downgrade state (must persist for `sustain_secs`).
        if !self.engaged && self.contention_ticks >= cfg.sustain_secs {
            self.engaged = true;
        }

        let mut decisions: Vec<Decision> = Vec::new();

        if self.engaged {
            if self.clear_ticks >= cfg.restore_after_secs {
                // 3. Contention cleared and sustained -> restore all,
                //    engine returns to idle.
                self.reset_counters();
                decisions.extend(self.take_all(RestoreReason::ContentionCleared));
            } else {
                // 4. Contention sustained -> downgrade new hot
                //    background processes.
                if contention {
                    for s in input.samples {
                        if s.cpu_percent < cfg.bg_cpu_threshold {
                            continue;
                        }
                        if Some(s.pid) == input.fg_pid {
                            continue; // Foreground processes are never downgraded.
                        }
                        if self.tracked.contains_key(&s.pid) {
                            continue; // Already downgraded.
                        }
                        if is_protected(
                            &s.name,
                            s.image_path.as_deref(),
                            &cfg.whitelist,
                            input.protecting_rules,
                        ) {
                            continue;
                        }
                        decisions.push(Decision::Downgrade {
                            pid: s.pid,
                            name: s.name.clone(),
                            cpu_percent: s.cpu_percent,
                        });
                    }
                }

                // 5. Per-process downgrade timeout -> restore (if still
                //    hot, the next round can downgrade it again).
                let now = input.now_secs;
                let expired: Vec<u32> = self
                    .tracked
                    .iter()
                    .filter(|(_, t)| now.saturating_sub(t.since_secs) >= cfg.max_downgrade_secs)
                    .map(|(pid, _)| *pid)
                    .collect();
                for pid in expired {
                    if let Some(t) = self.tracked.remove(&pid) {
                        decisions.push(Decision::Restore {
                            pid,
                            name: t.name,
                            originals: t.originals,
                            reason: RestoreReason::Timeout,
                        });
                    }
                }
            }
        }

        // 6. Process-exit cleanup (whether engaged or not): disappeared
        //    from the sample = exited.
        let alive: std::collections::HashSet<u32> =
            input.samples.iter().map(|s| s.pid).collect();
        let gone: Vec<u32> = self
            .tracked
            .keys()
            .filter(|pid| !alive.contains(pid))
            .copied()
            .collect();
        for pid in gone {
            if let Some(t) = self.tracked.remove(&pid) {
                decisions.push(Decision::Restore {
                    pid,
                    name: t.name,
                    originals: t.originals,
                    reason: RestoreReason::ProcessExited,
                });
            }
        }

        decisions
    }
}

// =========================================================================
// Protection check
// =========================================================================

/// Triple-layer protection check: system allowlist / user allowlist / rule
/// managed. `image_path` comes from the sample (only resolved when a Path
/// matching protecting rule exists) and is used for the full-path
/// Path-rule match. Passing None means Path rules will not match
/// (consistent with the rule engine semantics).
pub(super) fn is_protected(
    name: &str,
    image_path: Option<&str>,
    user_whitelist: &[String],
    protecting_rules: &[AffinityRule],
) -> bool {
    SYSTEM_WHITELIST.iter().any(|w| name_matches_exact(name, w))
        || user_whitelist.iter().any(|w| crate::matcher::wildcard_matches(name, w))
        || protecting_rules.iter().any(|r| rule_matches(r, name, image_path))
}

/// Case-insensitive, `.exe`-tolerant name match (reuses the rule engine's
/// exact-match semantics).
fn name_matches_exact(process_name: &str, pattern: &str) -> bool {
    crate::matcher::name_matches(process_name, pattern)
}
