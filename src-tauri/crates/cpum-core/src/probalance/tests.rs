//! Unit tests: pure state machine + config / log persistence (does not
//! touch Win32).

use super::*;
use crate::monitor::ProcSample;
use crate::procwin::ProcessPriorities;
use crate::rule::{AffinityRule, MatchType, RuleMode};

use super::engine::{cpu_above_downgrade_target, is_protected};
use super::journal::pair_orphans;

fn sample(pid: u32, name: &str, cpu: f32) -> ProcSample {
    ProcSample { pid, name: name.into(), cpu_percent: cpu, image_path: None }
}

fn sample_with_path(pid: u32, name: &str, cpu: f32, path: &str) -> ProcSample {
    ProcSample { pid, name: name.into(), cpu_percent: cpu, image_path: Some(path.into()) }
}

fn prios(pc: u32, io: u32) -> ProcessPriorities {
    ProcessPriorities { priority_class: Some(pc), io_priority: Some(io), memory_priority: Some(5) }
}

fn enabled_config() -> ProBalanceConfig {
    ProBalanceConfig {
        enabled: true,
        fg_cpu_threshold: 100.0,
        bg_cpu_threshold: 40.0,
        sustain_secs: 3,
        restore_after_secs: 2,
        max_downgrade_secs: 600,
        whitelist: vec![],
        game_mode_enabled: false,
    }
}

fn tick(
    engine: &mut ProBalanceEngine,
    now: u64,
    fg: Option<u32>,
    samples: &[ProcSample],
) -> Vec<Decision> {
    let input = TickInput { now_secs: now, fg_pid: fg, samples, protecting_rules: &[] };
    engine.tick(&input)
}

fn downgraded_pids(decisions: &[Decision]) -> Vec<u32> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Downgrade { pid, .. } => Some(*pid),
            _ => None,
        })
        .collect()
}

fn restored_pids(decisions: &[Decision]) -> Vec<u32> {
    decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Restore { pid, .. } => Some(*pid),
            _ => None,
        })
        .collect()
}

/// ROADMAP M3 acceptance: foreground fully loaded + background heavy load
/// -> downgrade within the threshold window; restore when contention
/// clears.
#[test]
fn acceptance_foreground_load_downgrades_background_then_restores() {
    let mut e = ProBalanceEngine::new(enabled_config());
    let fg = 1;
    let bg = 2;

    // First 2 seconds (sustain=3 not yet reached): no downgrade.
    for t in 1..=2u64 {
        let d = tick(&mut e, t, Some(fg), &[sample(fg, "game.exe", 300.0), sample(bg, "compiler.exe", 500.0)]);
        assert!(downgraded_pids(&d).is_empty(), "t={t} should not downgrade");
    }
    // 3rd second: contention threshold reached -> background downgraded.
    let d = tick(&mut e, 3, Some(fg), &[sample(fg, "game.exe", 300.0), sample(bg, "compiler.exe", 500.0)]);
    assert_eq!(downgraded_pids(&d), vec![bg]);
    e.mark_downgraded(bg, "compiler.exe".into(), prios(0x20, 2), 3);

    // Contention sustained: do not re-downgrade.
    let d = tick(&mut e, 4, Some(fg), &[sample(fg, "game.exe", 300.0), sample(bg, "compiler.exe", 500.0)]);
    assert!(downgraded_pids(&d).is_empty());

    // Contention cleared for 1 second (restore_after=2 not yet reached):
    // no restore.
    let d = tick(&mut e, 5, Some(fg), &[sample(fg, "game.exe", 5.0), sample(bg, "compiler.exe", 500.0)]);
    assert!(restored_pids(&d).is_empty());

    // 2nd second of cleared contention: all restored.
    let d = tick(&mut e, 6, Some(fg), &[sample(fg, "game.exe", 5.0), sample(bg, "compiler.exe", 500.0)]);
    assert_eq!(restored_pids(&d), vec![bg]);
    assert!(!e.is_engaged());
    assert_eq!(e.tracked_count(), 0);
}

#[test]
fn foreground_whitelist_and_rule_managed_never_downgraded() {
    let mut cfg = enabled_config();
    cfg.whitelist = vec!["precious*".into()];
    let mut e = ProBalanceEngine::new(cfg);

    // Already past sustain, so we are in the downgrade state.
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0)]);
    }
    assert!(e.is_engaged());

    let d = tick(
        &mut e,
        4,
        Some(1),
        &[
            sample(1, "game.exe", 300.0),      // foreground
            sample(2, "compiler.exe", 500.0),  // regular background -> downgrade
            sample(3, "dwm.exe", 500.0),       // system allowlist
            sample(4, "precious_app.exe", 500.0), // user allowlist (wildcard)
        ],
    );
    assert_eq!(downgraded_pids(&d), vec![2]);

    // Rule-managed priorities processes are also protected.
    let mut e2 = ProBalanceEngine::new(enabled_config());
    for t in 1..=3u64 {
        tick(&mut e2, t, Some(1), &[sample(1, "game.exe", 300.0)]);
    }
    let managed_rule = AffinityRule {
        id: "r1".into(),
        process_name: "compile*".into(),
        mask: "0xFF".into(),
        group_masks: None,
        enabled: true,
        created_at: 0,
        note: String::new(),
        match_type: MatchType::Wildcard,
        mode: RuleMode::Strict,
        priority_class: Some(0x80),
        io_priority: None,
        memory_priority: None,
    };
    let input = TickInput {
        now_secs: 4,
        fg_pid: Some(1),
        samples: &[sample(1, "game.exe", 300.0), sample(5, "compile_worker.exe", 500.0)],
        protecting_rules: &[managed_rule],
    };
    let d = e2.tick(&input);
    assert!(downgraded_pids(&d).is_empty(), "rule-managed processes must not be downgraded");
}

#[test]
fn timeout_restores_individual_process() {
    let mut e = ProBalanceEngine::new(enabled_config());
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    }
    let _ = tick(&mut e, 3, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    e.mark_downgraded(2, "bg.exe".into(), prios(0x20, 2), 3);

    // 600 seconds later: timeout restore (contention still active).
    let d = tick(&mut e, 603, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    assert_eq!(restored_pids(&d), vec![2]);
    match &d[0] {
        Decision::Restore { reason, .. } => assert_eq!(*reason, RestoreReason::Timeout),
        _ => panic!("expected Restore"),
    }
}

#[test]
fn exited_process_logged_without_writeback() {
    let mut e = ProBalanceEngine::new(enabled_config());
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    }
    e.mark_downgraded(2, "bg.exe".into(), prios(0x20, 2), 3);

    // PID 2 disappears from the samples -> ProcessExited restore decision.
    let d = tick(&mut e, 4, Some(1), &[sample(1, "game.exe", 300.0)]);
    assert_eq!(restored_pids(&d), vec![2]);
    match &d[0] {
        Decision::Restore { reason, .. } => assert_eq!(*reason, RestoreReason::ProcessExited),
        _ => panic!("expected Restore"),
    }
    assert_eq!(e.tracked_count(), 0);
}

#[test]
fn disable_config_returns_all_and_stops() {
    let mut e = ProBalanceEngine::new(enabled_config());
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    }
    e.mark_downgraded(2, "bg.exe".into(), prios(0x20, 2), 3);

    let mut disabled = enabled_config();
    disabled.enabled = false;
    let d = e.update_config(disabled);
    assert_eq!(restored_pids(&d), vec![2]);
    match &d[0] {
        Decision::Restore { reason, .. } => assert_eq!(*reason, RestoreReason::Disabled),
        _ => panic!("expected Restore"),
    }

    // After disable, ticks produce nothing.
    let d = tick(&mut e, 10, Some(1), &[sample(1, "game.exe", 300.0)]);
    assert!(d.is_empty());
}

#[test]
fn re_engage_requires_sustain_again() {
    let mut e = ProBalanceEngine::new(enabled_config());
    // First round: engage + downgrade + contention clears -> restore.
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    }
    e.mark_downgraded(2, "bg.exe".into(), prios(0x20, 2), 3);
    for t in 4..=6u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 5.0), sample(2, "bg.exe", 100.0)]);
    }
    assert!(!e.is_engaged());

    // Contention returns: must re-sustain for sustain_secs; no immediate
    // downgrade.
    let d = tick(&mut e, 7, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    assert!(downgraded_pids(&d).is_empty());
    let d = tick(&mut e, 8, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    assert!(downgraded_pids(&d).is_empty());
    let d = tick(&mut e, 9, Some(1), &[sample(1, "game.exe", 300.0), sample(2, "bg.exe", 100.0)]);
    assert_eq!(downgraded_pids(&d), vec![2]);
}

#[test]
fn no_foreground_means_no_contention() {
    let mut e = ProBalanceEngine::new(enabled_config());
    for t in 1..=10u64 {
        let d = tick(&mut e, t, None, &[sample(2, "bg.exe", 900.0)]);
        assert!(downgraded_pids(&d).is_empty(), "no foreground -> no downgrade");
    }
}

#[test]
fn game_mode_reuses_background_suppression_without_cpu_threshold() {
    let mut cfg = enabled_config();
    cfg.sustain_secs = 1;
    let mut engine = ProBalanceEngine::new(cfg);
    let input = TickInput {
        now_secs: 1,
        fg_pid: Some(1),
        samples: &[sample(1, "game.exe", 1.0), sample(2, "compiler.exe", 100.0)],
        protecting_rules: &[],
    };
    let decisions = engine.tick_with_game_mode(&input, true);
    assert_eq!(downgraded_pids(&decisions), vec![2]);
    assert!(engine.is_engaged());
}

// ---------- Configuration persistence ----------

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cpum-pb-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn config_roundtrip_and_defaults() {
    let dir = temp_dir();
    // No file -> default (disabled).
    assert!(!load_config(&dir).unwrap().enabled);

    let cfg = enabled_config();
    save_config(&dir, &cfg).unwrap();
    assert_eq!(load_config(&dir).unwrap(), cfg);

    // Validation: out-of-range values are rejected.
    let mut bad = enabled_config();
    bad.sustain_secs = 0;
    assert!(save_config(&dir, &bad).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn log_append_and_read_tail() {
    let dir = temp_dir();
    for i in 0..10 {
        let _ = append_log(
            &dir,
            &PbLogEntry {
                ts: i,
                action: "downgrade".into(),
                reason: None,
                pid: i as u32,
                name: "x.exe".into(),
                cpu: Some(50.0),
                from: None,
                to: None,
            },
        );
    }
    let tail = read_log(&dir, 3);
    assert_eq!(tail.len(), 3);
    assert_eq!(tail[0].pid, 7);
    assert_eq!(tail[2].pid, 9);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn system_whitelist_protects_critical_processes() {
    for name in ["csrss.exe", "DWM.exe", "svchost.exe", "cpum_service.exe", "Memory Compression"] {
        assert!(is_protected(name, None, &[], &[]), "{name} should be on the system allowlist");
    }
    assert!(!is_protected("compiler.exe", None, &[], &[]));
    assert!(is_protected("mytool.exe", None, &["mytool".into()], &[]));
}

// ---------- P0: downgrade determination must use scheduling order ----------

/// Regression test: numeric comparison would miss NORMAL (0x20) / HIGH
/// (0x80) / REALTIME (0x100) (all numerically < BELOW_NORMAL=0x4000),
/// i.e. ~99% of background processes would not be downgraded.
#[test]
fn downgrade_target_comparison_uses_scheduler_order() {
    assert!(!cpu_above_downgrade_target(0x40)); // IDLE: already below the target
    assert!(!cpu_above_downgrade_target(0x4000)); // BELOW_NORMAL: already the target
    assert!(cpu_above_downgrade_target(0x20)); // NORMAL (numerically 0x20 < 0x4000!)
    assert!(cpu_above_downgrade_target(0x8000)); // ABOVE_NORMAL
    assert!(cpu_above_downgrade_target(0x80)); // HIGH (numerically 0x80 < 0x4000!)
    assert!(cpu_above_downgrade_target(0x100)); // REALTIME (numerically 0x100 < 0x4000!)
    // Unknown values (EcoQoS background flags, etc.): conservatively do
    // not downgrade.
    assert!(!cpu_above_downgrade_target(0x0010_0000));
    assert!(!cpu_above_downgrade_target(0));
}

// ---------- P1: Path matching protecting rules ----------

#[test]
fn path_rule_protects_process_by_image_path() {
    let mut e = ProBalanceEngine::new(enabled_config());
    for t in 1..=3u64 {
        tick(&mut e, t, Some(1), &[sample(1, "game.exe", 300.0)]);
    }
    // A Path-matching rule that manages priorities: it only protects
    // processes under the matching path.
    let path_rule = AffinityRule {
        id: "r-path".into(),
        process_name: r"c:\tools\*\compile.exe".into(),
        mask: "0xFF".into(),
        group_masks: None,
        enabled: true,
        created_at: 0,
        note: String::new(),
        match_type: MatchType::Path,
        mode: RuleMode::Strict,
        priority_class: Some(0x80),
        io_priority: None,
        memory_priority: None,
    };
    let input = TickInput {
        now_secs: 4,
        fg_pid: Some(1),
        samples: &[
            sample(1, "game.exe", 300.0),
            sample_with_path(2, "compile.exe", 500.0, r"c:\tools\special\compile.exe"), // protected
            sample_with_path(3, "compile.exe", 500.0, r"d:\other\compile.exe"),         // not matched
            sample(4, "compile.exe", 500.0), // no path: Path rule misses -> downgraded
        ],
        protecting_rules: &[path_rule],
    };
    let d = e.tick(&input);
    let pids = downgraded_pids(&d);
    assert_eq!(pids, vec![3, 4], "only the path-matched process is protected");
}

// ---------- P1: log rotation read ----------

#[test]
fn read_log_spans_rotated_and_current_files() {
    let dir = temp_dir();
    // 8 entries in `.old` (older) + 4 in the current file (newer),
    // limit=6: must span both files and take the tail (4 newer + last 2
    // from `.old`).
    let make = |i: u64| PbLogEntry {
        ts: i,
        action: "downgrade".into(),
        reason: None,
        pid: i as u32,
        name: "x.exe".into(),
        cpu: None,
        from: None,
        to: None,
    };
    let old: String = (0..8)
        .map(|i| serde_json::to_string(&make(i)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join(format!("{PB_LOG_FILE}.old")), format!("{old}\n")).unwrap();
    let cur: String = (8..12)
        .map(|i| serde_json::to_string(&make(i)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.join(PB_LOG_FILE), format!("{cur}\n")).unwrap();

    let tail = read_log(&dir, 6);
    assert_eq!(tail.len(), 6);
    assert_eq!(tail.iter().map(|e| e.pid).collect::<Vec<_>>(), vec![6, 7, 8, 9, 10, 11]);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------- P2: startup reconciliation pairing ----------

#[test]
fn pair_orphans_detects_unpaired_downgrades() {
    let entry = |ts: u64, action: &str, pid: u32, name: &str, from: Option<ProcessPriorities>| {
        PbLogEntry {
            ts,
            action: action.into(),
            reason: None,
            pid,
            name: name.into(),
            cpu: None,
            from,
            to: None,
        }
    };
    let entries = vec![
        entry(1, "downgrade", 10, "a.exe", Some(prios(0x20, 2))), // orphan
        entry(2, "downgrade", 20, "b.exe", Some(prios(0x20, 2))), // already restored
        entry(3, "restore", 20, "b.exe", None),
        entry(4, "downgrade", 30, "c.exe", Some(prios(0x80, 2))), // already exited
        entry(5, "exit", 30, "c.exe", None),
        entry(6, "downgrade", 40, "d.exe", Some(prios(0x20, 2))), // two rounds: down->up->down
        entry(7, "restore", 40, "d.exe", None),
        entry(8, "downgrade", 40, "d.exe", Some(prios(0x4000, 2))), // orphan (last round's original)
    ];
    let orphans = pair_orphans(&entries);
    assert_eq!(orphans.len(), 2, "only PID 10 and 40 are orphans");
    assert_eq!(orphans.get(&10).unwrap().0, "a.exe");
    let (name40, prios40) = orphans.get(&40).unwrap();
    assert_eq!(name40, "d.exe");
    assert_eq!(prios40.priority_class, Some(0x4000), "use the original from the last round");

    // A `downgrade` missing the `from` field (corrupt data): not counted
    // as an orphan; never blindly write back.
    let bad = vec![entry(9, "downgrade", 50, "e.exe", None)];
    assert!(pair_orphans(&bad).is_empty());
}
