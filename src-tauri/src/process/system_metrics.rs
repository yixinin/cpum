//! Low-frequency system logical-processor utilization sampling.
//!
//! `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` returns
//! cumulative idle/kernel/user times per LP. This module keeps one baseline
//! and returns utilization deltas, so callers can poll at the UI cadence.

use std::mem::size_of;
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::Lazy;
use serde::Serialize;
use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessorPerformanceInformation,
};
use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;

#[derive(Clone, Serialize)]
pub struct LogicalProcessorUsage {
    pub index: u32,
    pub usage_percent: f32,
}

#[derive(Clone)]
struct Sample {
    idle: i64,
    kernel: i64,
    user: i64,
}

static PREVIOUS: Lazy<Mutex<Option<(Instant, Vec<Sample>)>>> = Lazy::new(|| Mutex::new(None));

fn read_raw() -> Result<Vec<Sample>, String> {
    let count = super::sampling::get_number_of_processors() as usize;
    let mut raw = vec![SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION::default(); count];
    let mut returned = 0u32;
    let status = unsafe {
        NtQuerySystemInformation(
            SystemProcessorPerformanceInformation,
            raw.as_mut_ptr() as *mut _,
            (raw.len() * size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>()) as u32,
            &mut returned,
        )
    };
    if status.0 < 0 {
        return Err(format!("NtQuerySystemInformation failed: NTSTATUS=0x{:08X}", status.0 as u32));
    }
    let valid = (returned as usize / size_of::<SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION>()).min(raw.len());
    Ok(raw[..valid]
        .iter()
        .map(|item| Sample { idle: item.IdleTime, kernel: item.KernelTime, user: item.UserTime })
        .collect())
}

/// Per-LP busy percentages. The first sample returns zeros because it only
/// establishes a baseline.
pub fn sample_logical_processor_usage() -> Result<Vec<LogicalProcessorUsage>, String> {
    let now = Instant::now();
    let current = read_raw()?;
    let mut previous = PREVIOUS.lock().map_err(|error| error.to_string())?;
    let usages = previous.as_ref().map_or_else(
        || current.iter().enumerate().map(|(index, _)| LogicalProcessorUsage { index: index as u32, usage_percent: 0.0 }).collect(),
        |(_, old)| current.iter().enumerate().map(|(index, item)| {
            let old = old.get(index);
            let total = old.map_or(0, |value| (item.kernel - value.kernel).saturating_add(item.user - value.user));
            let idle = old.map_or(0, |value| (item.idle - value.idle).max(0));
            let usage_percent = if total > 0 { (100.0 * (1.0 - idle as f64 / total as f64)).clamp(0.0, 100.0) as f32 } else { 0.0 };
            LogicalProcessorUsage { index: index as u32, usage_percent }
        }).collect(),
    );
    *previous = Some((now, current));
    Ok(usages)
}
