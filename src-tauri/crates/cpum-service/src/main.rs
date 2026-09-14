//! Windows service: CPU affinity rule daemon + ProBalance dynamic optimizer.
//!
//! Runs as SYSTEM after install:
//! - Every 5 seconds, scans processes and automatically applies affinity
//!   rules (mask / CPU Sets / three priority classes).
//! - Every second, runs a ProBalance decision tick: detect foreground
//!   contention, downgrade hot background processes (CPU BelowNormal +
//!   very low IO), and auto-restore when contention clears / the downgrade
//!   times out / the process exits.
//!
//! The rules file and the ProBalance config path are passed as service
//! startup arguments (set automatically by the Tauri installer command).
//! All rule loading / matching / application reuses the cpum-core engine -
//! the GUI's "Apply rules" button goes through the same implementation, so
//! behavior stays consistent by construction.
//!
//! Coordination with the rule engine: enabled rules that manage priorities
//! form a "protected list"; matching processes are skipped by ProBalance to
//! avoid the two engines clobbering each other's priority settings.
//!
//! This crate is intentionally separate from the main `cpum` GUI crate so
//! that `tauri_build::try_build` does not run its build script (and therefore
//! does not validate the bundled `cpum_service.exe` resource) when only the
//! service binary is being compiled.
//!
//! Besides the daemon, the binary doubles as a one-shot privileged helper:
//! when the GUI runs without the service installed it re-launches this binary
//! elevated with `--set-affinity` / `--set-priority`, so a single UAC prompt
//! can still cover protected processes.

use std::ffi::OsString;
use std::time::Duration;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode,
    ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::Result as WinSvcResult;

use cpum_core::engine;
use cpum_core::monitor::CpuSampler;
use cpum_core::probalance::ProBalanceRuntime;
use cpum_core::rule::AffinityRule;

const SERVICE_NAME: &str = "CpumAffinityService";
// Legacy machine-level directory, only used when the service is started
// without the rules directory argument (the desktop app always passes the
// per-user directory `%APPDATA%\com.eason.cpum` when installing the service).
const LEGACY_RULES_DIR: &str = r"C:\ProgramData\cpum";

/// Main loop tick interval (seconds) - ProBalance decision granularity.
const TICK_INTERVAL_SECS: u64 = 1;
/// Rule application interval (in ticks) - keeps the original 5-second cadence.
const RULE_APPLY_TICKS: u64 = 5;

// ---------- Windows Service main loop ----------

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    // ServiceMain's argv[0] is the service name; the rules directory written
    // by the install command comes in starting from argv[1].
    let rules_dir = arguments
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(LEGACY_RULES_DIR));

    if let Err(e) = run_service(rules_dir) {
        eprintln!("Service fatal: {e}");
    }
}

fn run_service(rules_dir: std::path::PathBuf) -> WinSvcResult<()> {
    cpum_core::procwin::enable_debug_privilege()
        .map_err(|message| windows_service::Error::Winapi(std::io::Error::other(message)))?;

    // Serve privileged requests from the (un-elevated) GUI. The listener blocks
    // on a background thread and dies with the process when the service stops.
    let bridge_dir = rules_dir.clone();
    std::thread::spawn(move || cpum_core::ipc::serve(bridge_dir));

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let status_handle = service_control_handler::register(
        SERVICE_NAME,
        move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        },
    )?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Main loop: one ProBalance tick per second; every 5 ticks apply rules
    // and refresh the protected list.
    // - The rule engine only counts failures for a single rule/process and
    //   does not abort the run; we ignore the return value and retry next
    //   round.
    // - When ProBalance is disabled, only write a status heartbeat (proves
    //   the service is alive) - no sampling, no decisions.
    // - The protected list = enabled rules that manage priorities; matching
    //   processes are never downgraded by ProBalance.
    let mut pb = ProBalanceRuntime::new(&rules_dir);
    let mut sampler = CpuSampler::new();
    let mut protecting_rules: Vec<AffinityRule> = Vec::new();
    let mut tick_count: u64 = 0;

    loop {
        if tick_count % RULE_APPLY_TICKS == 0 {
            match cpum_core::store::load_rules(&rules_dir) {
                Ok(rules) => {
                    protecting_rules = rules
                        .iter()
                        .filter(|r| r.enabled && r.manages_priorities())
                        .cloned()
                        .collect();
                    if let Err(e) = engine::apply_rules(&rules) {
                        eprintln!("Apply rules failed: {e}");
                    }
                }
                Err(e) => eprintln!("Load rules failed: {e}"),
            }
        }

        pb.tick(&rules_dir, &mut sampler, &protecting_rules);

        tick_count = tick_count.wrapping_add(1);
        if shutdown_rx.recv_timeout(Duration::from_secs(TICK_INTERVAL_SECS)).is_ok() {
            break;
        }
    }

    // Before stopping, restore all processes downgraded by ProBalance
    // (write back the original priorities + log the action).
    pb.shutdown(&rules_dir);

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

// ---------- Entry point ----------

// ---------- One-shot privileged helper ----------
// The GUI runs un-elevated. When it cannot modify a process itself and the
// service is not installed, it re-launches this binary elevated with one of the
// subcommands below, so the user gets exactly one UAC prompt for that action.

fn require_debug_privilege() -> Result<(), String> {
    // An elevated administrator token holds SeDebugPrivilege but starts with it
    // disabled, so it has to be turned on explicitly.
    cpum_core::procwin::enable_debug_privilege()
}

/// `--set-affinity <pid> <mask-csv> <strict|soft>`
fn one_shot_set_affinity(args: &[String]) -> Result<String, String> {
    require_debug_privilege()?;
    let pid: u32 = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "usage: --set-affinity <pid> <mask-csv> <strict|soft>".to_string())?;
    let masks: Vec<String> = args
        .get(3)
        .map(|value| value.split(',').map(|part| part.trim().to_string()).collect())
        .unwrap_or_default();
    let mode = match args.get(4).map(|value| value.as_str()) {
        Some("soft") => cpum_core::rule::RuleMode::Soft,
        _ => cpum_core::rule::RuleMode::Strict,
    };
    let parsed = cpum_core::procwin::GroupMasks::from_hex_list(&masks)?;
    cpum_core::procwin::set_affinity_by_group_masks(pid, &parsed.0, mode)?;
    Ok(format!("affinity applied to PID {pid}"))
}

/// `--set-priority <pid> <class|-> <io|-> <mem|->`
fn one_shot_set_priority(args: &[String]) -> Result<String, String> {
    require_debug_privilege()?;
    let pid: u32 = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "usage: --set-priority <pid> <class|-> <io|-> <mem|->".to_string())?;
    let parse = |index: usize| -> Option<u32> {
        args.get(index).and_then(|value| value.trim().parse::<u32>().ok())
    };
    if let Some(value) = parse(3) {
        cpum_core::procwin::set_process_priority_class(pid, value)?;
    }
    if let Some(value) = parse(4) {
        cpum_core::procwin::set_process_io_priority(pid, value)?;
    }
    if let Some(value) = parse(5) {
        cpum_core::procwin::set_process_memory_priority(pid, value)?;
    }
    Ok(format!("priorities applied to PID {pid}"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    // --apply-once <dir>  test mode: run once and exit
    if args.get(1).map(|s| s.as_str()) == Some("--apply-once") {
        let dir = args
            .get(2)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(LEGACY_RULES_DIR));
        let report = engine::apply_rules_from_dir(&dir)?;
        println!("Applied: {} ok, {} failed", report.applied, report.failed);
        return Ok(());
    }

    // Elevated one-shot helpers used as the GUI's last-resort fallback.
    match args.get(1).map(|s| s.as_str()) {
        Some("--set-affinity") => {
            match one_shot_set_affinity(&args) {
                Ok(message) => println!("{message}"),
                Err(message) => {
                    eprintln!("{message}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some("--set-priority") => {
            match one_shot_set_priority(&args) {
                Ok(message) => println!("{message}"),
                Err(message) => {
                    eprintln!("{message}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        _ => {}
    }

    // Normal mode: run as a Windows service.
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}
