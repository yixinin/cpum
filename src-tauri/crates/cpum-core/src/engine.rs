//! Rule application engine: enumerate processes -> match rules -> apply
//! (affinity + priorities) -> report results.
//!
//! The GUI's "Apply rules" button and the Windows service's 5-second poll
//! both flow through this function, guaranteeing that the two paths
//! produce identical effects for the same rule set.

use std::path::Path;

use crate::matcher::rule_matches;
use crate::procwin::{self, ProcessPriorities};
use crate::rule::{self, AffinityRule};
use crate::store;

/// Result of applying a single process (the GUI uses this to emit an event
/// and patch the row in place).
#[derive(Debug, Clone)]
pub struct ProcessApplyInfo {
    pub pid: u32,
    /// Affinity mask after application, as a hex string.
    /// In soft mode this is the system mask (hard mask has been released);
    /// in strict mode it is the rule mask.
    pub mask_hex: Option<String>,
    /// Actual values read back when the rule manages priorities (used by
    /// the GUI event). None when no priorities are managed.
    pub priorities: Option<ProcessPriorities>,
}

/// Summary of one `apply_rules` invocation.
#[derive(Debug, Default)]
pub struct ApplyReport {
    /// Number of successful (process x rule) applications.
    pub applied: u32,
    /// Number of failures (a single process failing does not abort the run).
    pub failed: u32,
    /// Per-process details of successful applications (used by the GUI
    /// event).
    pub changed: Vec<ProcessApplyInfo>,
}

/// Apply a list of rules to all currently-running processes.
///
/// - Pre-validation: if any enabled rule is invalid (mask cannot be parsed
///   or priority is out of range), this function returns an error directly
///   so the GUI can surface it to the user (matches the v1 behavior; the
///   service's polling ignores the error and continues).
/// - A failure applying a single process is only counted; it does not
///   affect other processes.
pub fn apply_rules(rules: &[AffinityRule]) -> Result<ApplyReport, String> {
    let active: Vec<&AffinityRule> = rules.iter().filter(|r| r.enabled).collect();
    if active.is_empty() {
        return Ok(ApplyReport::default());
    }
    for r in &active {
        rule::validate_rule(r)?;
    }

    // Only resolve full paths when a Path-type rule exists, to avoid
    // opening every process for nothing.
    let need_paths = active.iter().any(|r| r.match_type == rule::MatchType::Path);
    let processes = procwin::enumerate_processes(need_paths)?;

    let mut report = ApplyReport::default();
    for r in &active {
        let masks = r.group_masks.as_ref()
            .map(|values| procwin::GroupMasks::from_hex_list(values).expect("validate_rule guarantees valid masks").0)
            .unwrap_or_else(|| vec![procwin::parse_hex_mask(&r.mask).expect("validate_rule guarantees a valid mask")]);
        for entry in &processes {
            if !rule_matches(r, &entry.name, entry.path.as_deref()) {
                continue;
            }
            match apply_one(r, entry.pid, &masks) {
                Ok(info) => {
                    report.applied += 1;
                    report.changed.push(info);
                }
                Err(e) => {
                    report.failed += 1;
                    eprintln!("failed to apply rule {} to PID {}: {}", r.process_name, entry.pid, e);
                }
            }
        }
    }
    Ok(report)
}

/// Load rules from the given directory and apply them immediately (used
/// by the service's polling loop and the `--apply-once` test mode).
pub fn apply_rules_from_dir(base_dir: &Path) -> Result<ApplyReport, String> {
    let rules = store::load_rules(base_dir)?;
    apply_rules(&rules)
}

fn apply_one(rule: &AffinityRule, pid: u32, masks: &[u64]) -> Result<ProcessApplyInfo, String> {
    // 1. Affinity / CPU Sets (soft mode automatically falls back to the
    //    hard mask when the system doesn't support CPU Sets).
    let soft_applied = procwin::set_affinity_by_group_masks(pid, masks, rule.mode)?;

    // 2. The three priority classes (each independent; any one failing
    //    counts this process as a failure).
    if let Some(pc) = rule.priority_class {
        procwin::set_process_priority_class(pid, pc)?;
    }
    if let Some(io) = rule.io_priority {
        procwin::set_process_io_priority(pid, io)?;
    }
    if let Some(mp) = rule.memory_priority {
        procwin::set_process_memory_priority(pid, mp)?;
    }

    // 3. Read back the actual state (fed to the GUI event; in soft mode
    //    the hard mask == system mask).
    let mask_hex = if soft_applied {
        procwin::get_process_affinity(pid)
            .ok()
            .and_then(|(pm, _)| pm)
            .map(procwin::mask_to_hex)
    } else {
        Some(procwin::mask_to_hex(masks.first().copied().unwrap_or_default()))
    };
    let priorities = if rule.priority_class.is_some()
        || rule.io_priority.is_some()
        || rule.memory_priority.is_some()
    {
        Some(procwin::get_process_priorities(pid))
    } else {
        None
    };

    Ok(ProcessApplyInfo { pid, mask_hex, priorities })
}
