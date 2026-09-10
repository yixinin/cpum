//! Dynamic optimization engine (ProBalance): contention detection ->
//! background downgrade -> automatic restore.
//!
//! Architecture layers (consistent with the rule engine's "single shared
//! source" model):
//! - **Decision core** [`engine`]: a pure state machine. Input: a sample
//!   snapshot + foreground PID. Output: a list of [`Decision`]. Never
//!   touches Win32; can be fully unit-tested.
//! - **Execution layer** [`runtime`]: service-side orchestration - hot
//!   config reload / sampling / foreground detection / calling procwin to
//!   apply downgrades and restores / writing logs and status files.
//! - [`config`]: config model and persistence (shared by the GUI writer
//!   and the service hot-reloader).
//! - [`journal`]: action log (JSONL rotation) and status file + orphan
//!   downgrade reconciliation.
//!
//! Behavior model (mirroring Process Lasso's ProBalance):
//! 1. **Contention detection**: when the foreground process's CPU >=
//!    `fg_cpu_threshold` and remains there for `sustain_secs`, enter
//!    the downgrade state.
//! 2. **Action execution**: background processes whose CPU >=
//!    `bg_cpu_threshold` get CPU priority Below Normal and IO priority
//!    Very Low. Foreground processes, allowlisted processes, and
//!    processes whose priorities are managed by rules are never
//!    downgraded.
//! 3. **Restore**: contention cleared for `restore_after_secs` / a
//!    single-process downgrade times out / the process exits / the
//!    feature is disabled / the service stops - all automatically
//!    restored to the original value.
//! 4. **Guardrails**: system-critical processes are hardcoded in an
//!    allowlist; every action is written to the JSONL log for the GUI to
//!    read back.

mod config;
mod engine;
mod journal;
mod runtime;

#[cfg(test)]
mod tests;

pub use config::{
    load_config, save_config, ProBalanceConfig, PB_CONFIG_FILE, PB_CONFIG_VERSION,
};
pub use engine::{
    Decision, ProBalanceEngine, RestoreReason, TickInput, DOWNGRADE_IO_PRIORITY,
    DOWNGRADE_PRIORITY_CLASS, SYSTEM_WHITELIST,
};
pub use journal::{
    append_log, read_log, read_status, statistics, write_status, PbLogEntry, PbStatistics, PbStatus, PB_LOG_FILE,
    PB_LOG_ROTATE_BYTES, PB_STATUS_FILE,
};
pub use runtime::ProBalanceRuntime;
