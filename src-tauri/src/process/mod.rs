//! Process subsystem: enumeration, CPU affinity read/write and resource
//! utilization (CPU / Mem / Disk / Net) sampling.
//!
//! Module layout (by responsibility, bottom-up):
//! - [`sampling`]:   Sampling primitives - handle helpers, single-shot metric
//!                   reads, differential cache (foundation for rate metrics)
//! - [`enumerate`]:  Process enumeration - ToolHelp fast scan + parallel handle
//!                   walk, three-stage first-frame loading
//! - [`metrics`]:    Metrics stream - per-second sampling, 4-wave events + struct
//!                   diff, background thread control
//! - [`net_probe`]:  Network IO diagnostic probe (verifies NtQueryInformationProcess
//!                   support)
//!
//! Affinity / CPU Sets / the three priority classes are all read/written in
//! cpum-core (shared single implementation with the service).

mod enumerate;
mod metrics;
mod net_probe;
mod sampling;
mod system_metrics;

pub use enumerate::{
    list_processes, list_processes_light, load_processes_cache, save_processes_cache,
};
pub use metrics::{start_metrics_stream_in_thread, stop_metrics_stream_in_thread};
pub use system_metrics::{sample_logical_processor_usage, LogicalProcessorUsage};

pub fn build_affinity_updated_event(pid: u32, affinity_mask: Option<String>) -> serde_json::Value {
    serde_json::json!({ "pid": pid, "affinity_mask": affinity_mask })
}

// Unit Tests: parse_hex_mask / mask_to_hex tests have moved with the
// implementation to cpum-core::procwin. This module only keeps metric-related
// logic (its correctness depends on system calls and is verified manually).
