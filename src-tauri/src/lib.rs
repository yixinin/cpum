// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
// The single implementation of the rule model / matching / persistence /
// application engine lives in the cpum-core crate (shared by the GUI and the
// Windows service). This module is only a thin Tauri command layer.

mod models;
mod process;
mod topology;

use models::{mask_to_hex, CpuTopology, ProcessInfo};
use cpum_core::rule::{AffinityRule, MatchType, RuleMode};
use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use tauri::{Emitter, Manager, Runtime};
use uuid::Uuid;

#[tauri::command]
fn get_process_exe_path(pid: u32) -> Option<String> {
    cpum_core::procwin::get_process_exe_path(pid)
}

fn apply_priorities_locally(
    pid: u32, priority_class: Option<u32>, io_priority: Option<u32>, memory_priority: Option<u32>,
) -> Result<(), String> {
    if let Some(value) = priority_class { cpum_core::procwin::set_process_priority_class(pid, value)?; }
    if let Some(value) = io_priority { cpum_core::procwin::set_process_io_priority(pid, value)?; }
    if let Some(value) = memory_priority { cpum_core::procwin::set_process_memory_priority(pid, value)?; }
    Ok(())
}

#[tauri::command]
fn set_process_priority<R: Runtime>(
    app: tauri::AppHandle<R>, pid: u32, priority_class: Option<u32>, io_priority: Option<u32>, memory_priority: Option<u32>,
) -> Result<(), String> {
    let mut failure = apply_priorities_locally(pid, priority_class, io_priority, memory_priority).err();
    let mut privileged_result: Option<cpum_core::procwin::ProcessPriorities> = None;

    if let Some(error) = &failure {
        if is_access_denied_error(error) {
            match bridge_set_priority(&app, pid, priority_class, io_priority, memory_priority) {
                Ok(response) => {
                    privileged_result = response.priorities;
                    failure = None;
                }
                Err(bridge_error) => {
                    if !elevation_failed(pid) {
                        match elevated_set_priority(pid, priority_class, io_priority, memory_priority) {
                            Ok(()) => failure = None,
                            Err(elevated_error) => {
                                mark_elevation_failed(pid);
                                failure = Some(format!(
                                    "{error}\nvia service: {bridge_error}\nvia elevation: {elevated_error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(error) = failure {
        return Err(error);
    }

    // Prefer the values the service read back; reading them locally would fail
    // for exactly the processes that needed the service in the first place.
    let current = privileged_result
        .unwrap_or_else(|| cpum_core::procwin::get_process_priorities(pid));
    let _ = app.emit("process://priority-updated", serde_json::json!({ "pid": pid, "priority_class": current.priority_class, "io_priority": current.io_priority, "memory_priority": current.memory_priority }));
    Ok(())
}

/// Return the current system's CPU topology (logical processors / physical
/// cores / CCD).
#[tauri::command]
fn get_cpu_topology() -> Result<CpuTopology, String> {
    topology::get_cpu_topology()
}

/// Enumerate all processes with their current CPU affinity (called for the
/// first full-frame load; also writes the metrics sampling baseline).
/// When finished, the process list cache is written automatically, so the next
/// startup can read it in <1ms.
#[tauri::command]
fn list_processes<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<ProcessInfo>, String> {
    let result = process::list_processes()?;
    // Write cache asynchronously (failures are silently ignored)
    if let Ok(cache_dir) = app.path().app_data_dir() {
        process::save_processes_cache(&cache_dir, &result);
    }
    Ok(result)
}

/// Ultra-lightweight fast scan (PID / name / parent_pid only), returns in
/// <20ms, used to show first-frame content immediately.
#[tauri::command]
fn list_processes_light() -> Result<Vec<ProcessInfo>, String> {
    process::list_processes_light()
}

/// Load the process list cache from the previous run (<1ms). Returns an empty
/// Vec on the first run when no cache exists.
#[tauri::command]
fn list_processes_cached<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<ProcessInfo>, String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to get app_data_dir: {e}"))?;
    Ok(process::load_processes_cache(&cache_dir).unwrap_or_default())
}

// ---------- Privileged fallback ----------
// The app runs un-elevated, so it cannot modify processes owned by another
// account or running at a higher integrity level: OpenProcess fails with
// ERROR_ACCESS_DENIED because a filtered UAC token does not hold
// SeDebugPrivilege. Two escalation paths exist, tried in this order:
//   1. the LocalSystem service (already running -> no prompt at all);
//   2. a one-shot elevated launch of the bundled service binary (one UAC
//      prompt, only when the service is not installed).
// Processes that even SYSTEM may not touch (PPL / protected anti-cheat) still
// fail; those PIDs are remembered so we do not prompt again for them.

static ELEVATION_FAILED: Lazy<Mutex<HashSet<u32>>> = Lazy::new(|| Mutex::new(HashSet::new()));

fn elevation_failed(pid: u32) -> bool {
    ELEVATION_FAILED.lock().map(|set| set.contains(&pid)).unwrap_or(false)
}

fn mark_elevation_failed(pid: u32) {
    if let Ok(mut set) = ELEVATION_FAILED.lock() {
        set.insert(pid);
    }
}

/// Win32 reports a refused open as `ERROR_ACCESS_DENIED` (5); windows-rs
/// formats it as `Access is denied. (0x80070005)`.
fn is_access_denied_error(message: &str) -> bool {
    message.contains("0x80070005") || message.contains("Access is denied")
}

fn bridge_token<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<String, String> {
    cpum_core::ipc::ensure_token(&rules_dir(app)?)
}

fn bridge_response(response: cpum_core::ipc::Response) -> Result<cpum_core::ipc::Response, String> {
    if response.ok {
        Ok(response)
    } else {
        Err(response.error.unwrap_or_else(|| "service bridge refused the request".to_string()))
    }
}

/// Delegate a single affinity change to the LocalSystem service.
fn bridge_set_affinity<R: Runtime>(
    app: &tauri::AppHandle<R>, pid: u32, masks: &[String], mode: RuleMode,
) -> Result<(), String> {
    let request = cpum_core::ipc::Request::SetAffinity {
        token: bridge_token(app)?,
        pid,
        masks: masks.to_vec(),
        mode,
    };
    bridge_response(cpum_core::ipc::request(&request)?).map(|_| ())
}

/// Delegate one or more priority changes to the LocalSystem service.
fn bridge_set_priority<R: Runtime>(
    app: &tauri::AppHandle<R>,
    pid: u32,
    priority_class: Option<u32>,
    io_priority: Option<u32>,
    memory_priority: Option<u32>,
) -> Result<cpum_core::ipc::Response, String> {
    let request = cpum_core::ipc::Request::SetPriority {
        token: bridge_token(app)?,
        pid,
        priority_class,
        io_priority,
        memory_priority,
    };
    bridge_response(cpum_core::ipc::request(&request)?)
}

/// Ask the service to re-apply every enabled rule.
fn bridge_apply_rules<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<cpum_core::ipc::Response, String> {
    let request = cpum_core::ipc::Request::ApplyRules { token: bridge_token(app)? };
    bridge_response(cpum_core::ipc::request(&request)?)
}

/// Re-launch the bundled service binary elevated for a single operation.
/// This is the last-resort path: it shows one UAC prompt.
fn run_elevated_helper(arguments: &[String], description: &str) -> Result<String, String> {
    let helper = service_exe_path()?;
    let temp_bat = std::env::temp_dir().join(format!("cpum_elevate_{}.bat", Uuid::new_v4()));
    let temp_log = temp_bat.with_extension("log");

    let command_line = std::iter::once(format!("\"{}\"", helper.display()))
        .chain(arguments.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let bat_content = format!(
        "@echo off\r\n{command_line} > \"{log}\" 2>&1\r\nexit /b %errorlevel%\r\n",
        log = temp_log.display()
    );
    std::fs::write(&temp_bat, bat_content)
        .map_err(|e| format!("failed to create temp script: {e}"))?;

    let escaped_bat = temp_bat.display().to_string().replace('\'', "''");
    let ps_cmd = format!(
        "$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', '{escaped_bat}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode"
    );
    let output = run_command_with_deadline(
        Command::new("powershell.exe").args(["-NoProfile", "-Command", &ps_cmd]),
        description,
        ELEVATION_TIMEOUT,
    );
    let script_output = std::fs::read(&temp_log)
        .map(|bytes| decode_console_output(&bytes).trim().to_string())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&temp_bat);
    let _ = std::fs::remove_file(&temp_log);

    let output = output?;
    if output.status.success() {
        Ok(script_output)
    } else {
        Err(if script_output.is_empty() {
            command_output_text(&output)
        } else {
            script_output
        })
    }
}

fn elevated_set_affinity(pid: u32, masks: &[String], mode: RuleMode) -> Result<(), String> {
    let mode_text = match mode {
        RuleMode::Soft => "soft",
        _ => "strict",
    };
    run_elevated_helper(
        &[
            "--set-affinity".to_string(),
            pid.to_string(),
            masks.join(","),
            mode_text.to_string(),
        ],
        "elevated affinity change",
    )
    .map(|_| ())
}

fn elevated_set_priority(
    pid: u32,
    priority_class: Option<u32>,
    io_priority: Option<u32>,
    memory_priority: Option<u32>,
) -> Result<(), String> {
    let text = |value: Option<u32>| value.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
    run_elevated_helper(
        &[
            "--set-priority".to_string(),
            pid.to_string(),
            text(priority_class),
            text(io_priority),
            text(memory_priority),
        ],
        "elevated priority change",
    )
    .map(|_| ())
}

/// Set the CPU affinity mask of the specified process.
/// `mask` is passed as a hex string (e.g. "0xFF") to support the all-ones
/// 64-bit case.
#[tauri::command]
fn set_process_affinity<R: Runtime>(
    app: tauri::AppHandle<R>,
    pid: u32,
    mask: String,
    mode: Option<RuleMode>,
    group_masks: Option<Vec<String>>,
) -> Result<(), String> {
    let mode = mode.unwrap_or_default();
    let hex_masks: Vec<String> = group_masks.clone().unwrap_or_else(|| vec![mask.clone()]);
    let masks = match group_masks {
        Some(values) => cpum_core::procwin::GroupMasks::from_hex_list(&values)?.0,
        None => vec![cpum_core::procwin::parse_hex_mask(&mask)?],
    };

    let mut failure = cpum_core::procwin::set_affinity_by_group_masks(pid, &masks, mode).err();

    if let Some(error) = &failure {
        if is_access_denied_error(error) {
            match bridge_set_affinity(&app, pid, &hex_masks, mode) {
                Ok(()) => failure = None,
                Err(bridge_error) => {
                    if !elevation_failed(pid) {
                        match elevated_set_affinity(pid, &hex_masks, mode) {
                            Ok(()) => failure = None,
                            Err(elevated_error) => {
                                // Remember the PID: a protected process would
                                // otherwise trigger a UAC prompt on every retry.
                                mark_elevation_failed(pid);
                                failure = Some(format!(
                                    "{error}\nvia service: {bridge_error}\nvia elevation: {elevated_error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    let Some(error) = failure else {
        // On success, push an event to the frontend immediately so it can patch
        // the corresponding row's mask in place without a full table refresh.
        let new_mask_hex = masks.first().copied().map(mask_to_hex);
        let payload = process::build_affinity_updated_event(pid, new_mask_hex);
        let _ = app.emit("process://affinity-updated", payload);
        return Ok(());
    };
    Err(error)
}

// ---------- Metrics staged push stream ----------

/// Start the backend metrics push stream (one sample per second, split into
/// 4 waves + structural diff events to avoid the frontend table flickering).
#[tauri::command]
fn start_metrics_stream<R: Runtime>(
    app: tauri::AppHandle<R>,
    interval_ms: u32,
) -> Result<(), String> {
    let app_clone = app.clone();
    process::start_metrics_stream_in_thread(interval_ms, move |event, payload| {
        app_clone
            .emit(event, payload)
            .map_err(|e| format!("emit failed: {}", e))
    })
}

/// Stop the backend metrics push stream (paused).
#[tauri::command]
fn stop_metrics_stream() -> Result<(), String> {
    process::stop_metrics_stream_in_thread()
}

// ---------- CPU topology cache (faster startup; CPU topology almost never changes) ----------

/// Read cache: first compute a <1ms hardware signature (vendor + family +
/// model + LP count); only return the cache if the signature matches.
/// Otherwise return None so the frontend falls back to real detection.
/// Signature mismatches happen when the CPU or motherboard was swapped.
#[tauri::command]
fn load_cpu_topology_cache<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<topology::TopologyCache>, String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to get app_data_dir: {e}"))?;
    // First get the LP count quickly via GetSystemInfo (no CPUID pinning)
    let total_lps: u32 = topology::sys_info_logical_processor_count()
        .unwrap_or(0);
    let signature = topology::hw_signature_fast(total_lps);
    topology::load_topology_cache(&cache_dir, &signature)
}

/// Write cache: called after the topology backend has finished a full
/// detection so the next startup can read the cache (<1ms vs 1-2s).
#[tauri::command]
fn save_cpu_topology_cache<R: Runtime>(
    app: tauri::AppHandle<R>,
    topology: CpuTopology,
) -> Result<(), String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to get app_data_dir: {e}"))?;
    let total_lps = topology.total_logical_processors;
    let signature = topology::hw_signature_fast(total_lps);
    topology::save_topology_cache(&cache_dir, &topology, &signature)
}

// ---------- Affinity rules persistence ----------

fn affinity_rules_path(base_dir: &PathBuf) -> PathBuf {
    base_dir.join("affinity_rules.json")
}

/// Rules and ProBalance state live in the per-user Tauri data directory
/// (`%APPDATA%\<bundle identifier>`). The desktop app runs un-elevated
/// (`asInvoker`), so it must never write to machine-level locations such as
/// `%ProgramData%`. The Windows service receives this directory as its startup
/// argument and runs as `LocalSystem`, which can read it directly.
fn rules_dir<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to get app data dir: {e}"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create rules directory: {e}"))?;
    Ok(dir)
}

/// Read-only fallback sources from older releases. They are only used to
/// migrate existing data into the current per-user directory, never written to.
fn legacy_rules_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("cpum"));
    }
    if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
        dirs.push(PathBuf::from(program_data).join("cpum"));
    }
    dirs
}

#[tauri::command]
fn get_logical_processor_usage() -> Result<Vec<process::LogicalProcessorUsage>, String> {
    process::sample_logical_processor_usage()
}

#[tauri::command]
fn get_probalance_config<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<cpum_core::probalance::ProBalanceConfig, String> {
    cpum_core::probalance::load_config(&rules_dir(&app)?)
}

#[tauri::command]
fn save_probalance_config<R: Runtime>(
    app: tauri::AppHandle<R>,
    config: cpum_core::probalance::ProBalanceConfig,
) -> Result<(), String> {
    cpum_core::probalance::save_config(&rules_dir(&app)?, &config)
}

#[tauri::command]
fn get_probalance_status<R: Runtime>(app: tauri::AppHandle<R>) -> Option<cpum_core::probalance::PbStatus> {
    cpum_core::probalance::read_status(&rules_dir(&app).ok()?)
}

#[tauri::command]
fn get_probalance_log<R: Runtime>(app: tauri::AppHandle<R>, limit: usize) -> Vec<cpum_core::probalance::PbLogEntry> {
    let Some(dir) = rules_dir(&app).ok() else {
        return Vec::new();
    };
    cpum_core::probalance::read_log(&dir, limit.min(500))
}

#[tauri::command]
fn get_probalance_statistics<R: Runtime>(app: tauri::AppHandle<R>) -> cpum_core::probalance::PbStatistics {
    let Some(dir) = rules_dir(&app).ok() else {
        return Default::default();
    };
    cpum_core::probalance::statistics(&dir)
}

/// Save the affinity rule list.
#[tauri::command]
fn save_affinity_rules<R: Runtime>(
    app: tauri::AppHandle<R>,
    rules: Vec<AffinityRule>,
) -> Result<(), String> {
    for rule in &rules { cpum_core::rule::validate_rule(rule)?; }
    cpum_core::store::save_rules(&rules_dir(&app)?, &rules)
}

/// Load the affinity rule list.
#[tauri::command]
fn load_affinity_rules<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<AffinityRule>, String> {
    let primary_path = affinity_rules_path(&rules_dir(&app)?);
    let (path, raw) = match std::fs::read_to_string(&primary_path) {
        Ok(raw) => (primary_path.clone(), raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut found = None;
            for legacy_path in legacy_rules_dirs().iter().map(affinity_rules_path) {
                match std::fs::read_to_string(&legacy_path) {
                    Ok(raw) => {
                        found = Some((legacy_path, raw));
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("failed to read rules file: {error}")),
                }
            }
            let Some(found) = found else {
                return Ok(vec![]);
            };
            found
        }
        Err(error) => return Err(format!("failed to read rules file: {error}")),
    };
    let rules = cpum_core::store::parse_rules(&raw)?;

    // When rules are read from a legacy location, migrate them into the
    // per-user directory that the service is pointed at.
    if path != primary_path {
        save_affinity_rules(app, rules.clone())?;
    }

    Ok(rules)
}

/// Add one affinity rule.
#[tauri::command]
fn add_affinity_rule<R: Runtime>(
    app: tauri::AppHandle<R>,
    process_name: String,
    mask: String,
    note: String,
    match_type: Option<MatchType>,
    mode: Option<RuleMode>,
    priority_class: Option<u32>,
    io_priority: Option<u32>,
    memory_priority: Option<u32>,
    group_masks: Option<Vec<String>>,
) -> Result<AffinityRule, String> {
    let mut rules = load_affinity_rules(app.clone())?;

    let mut rule = cpum_core::store::build_rule(cpum_core::store::RuleDraft {
        process_name, mask, note, match_type: match_type.unwrap_or_default(), mode: mode.unwrap_or_default(),
        priority_class, io_priority, memory_priority,
    })?;
    if let Some(group_masks) = group_masks { rule.group_masks = Some(group_masks); cpum_core::rule::validate_rule(&rule)?; }

    rules.push(rule.clone());
    save_affinity_rules(app, rules)?;

    Ok(rule)
}

/// Update one affinity rule.
#[tauri::command]
fn update_affinity_rule<R: Runtime>(
    app: tauri::AppHandle<R>,
    rule: AffinityRule,
) -> Result<AffinityRule, String> {
    let mut rules = load_affinity_rules(app.clone())?;

    cpum_core::rule::validate_rule(&rule)?;
    let existing = rules.iter_mut().find(|existing| existing.id == rule.id)
        .ok_or_else(|| format!("rule not found: {}", rule.id))?;
    *existing = rule.clone();
    let updated = rule;
    save_affinity_rules(app, rules)?;

    Ok(updated)
}

/// Delete one affinity rule.
#[tauri::command]
fn delete_affinity_rule<R: Runtime>(
    app: tauri::AppHandle<R>,
    id: String,
) -> Result<(), String> {
    let mut rules = load_affinity_rules(app.clone())?;
    rules.retain(|r| r.id != id);
    save_affinity_rules(app, rules)?;
    Ok(())
}

/// Apply all enabled affinity rules to the currently running processes.
/// Returns the number of processes the rules were applied to.
#[tauri::command]
fn apply_affinity_rules<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<u32, String> {
    let rules = load_affinity_rules(app.clone())?;
    let report = cpum_core::engine::apply_rules(&rules)?;
    let mut applied = report.applied;
    let mut handled: HashSet<u32> = HashSet::new();
    for changed in report.changed {
        handled.insert(changed.pid);
        let _ = app.emit("process://affinity-updated", process::build_affinity_updated_event(changed.pid, changed.mask_hex));
        if let Some(priorities) = changed.priorities {
            let _ = app.emit("process://priority-updated", serde_json::json!({ "pid": changed.pid, "priority_class": priorities.priority_class, "io_priority": priorities.io_priority, "memory_priority": priorities.memory_priority }));
        }
    }

    // Processes the un-elevated app could not open are re-applied by the
    // service (best effort - it is an optional component). This path is
    // deliberately not backed by a UAC prompt: a batch operation should not
    // raise one per protected process, and the per-process editor already
    // offers elevation for individual PIDs.
    if report.failed > 0 {
        if let Ok(response) = bridge_apply_rules(&app) {
            if let Some(changed) = response.changed {
                for entry in changed {
                    if !handled.insert(entry.pid) {
                        continue;
                    }
                    applied += 1;
                    let _ = app.emit("process://affinity-updated", process::build_affinity_updated_event(entry.pid, entry.mask_hex));
                    if let Some(priorities) = entry.priorities {
                        let _ = app.emit("process://priority-updated", serde_json::json!({ "pid": entry.pid, "priority_class": priorities.priority_class, "io_priority": priorities.io_priority, "memory_priority": priorities.memory_priority }));
                    }
                }
            }
        }
    }

    Ok(applied)
}

/// Auto-generate a unique ID for an affinity rule.
#[tauri::command]
fn generate_affinity_rule_id() -> String {
    Uuid::new_v4().to_string()
}

// ==========================================================================
// Windows service management (cpum_service.exe)
// ==========================================================================

/// Locate the service executable shipped with the installer bundle.
/// Tauri places `bundle.resources` under the Windows install directory's
/// `resources` subdirectory.
fn service_exe_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("failed to get current exe path: {e}"))?;
    let dir = exe.parent().ok_or("cannot determine exe parent directory")?;
    let candidates = [
        dir.join("resources").join("cpum_service.exe"),
        // Compatible with dev environment and older manual deployments.
        dir.join("cpum_service.exe"),
    ];

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "service executable not found. Please reinstall a build that includes cpum_service.exe (checked: {})",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

const SERVICE_NAME: &str = "CpumAffinityService";
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// An elevated launch waits for the user to react to the UAC prompt, which can
/// easily take longer than the short timeout used for plain `sc.exe` queries.
const ELEVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run_command_with_timeout(command: &mut Command, description: &str) -> Result<Output, String> {
    run_command_with_deadline(command, description, COMMAND_TIMEOUT)
}

fn run_command_with_deadline(
    command: &mut Command, description: &str, timeout: std::time::Duration,
) -> Result<Output, String> {
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start {description}: {e}"))?;
    let deadline = std::time::Instant::now() + timeout;

    loop {
        if child
            .try_wait()
            .map_err(|e| format!("failed while waiting for {description}: {e}"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|e| format!("failed to read {description} output: {e}"));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{description} timed out ({} seconds)", timeout.as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn command_output_text(output: &Output) -> String {
    let stdout = decode_console_output(&output.stdout).trim().to_string();
    let stderr = decode_console_output(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(command did not return details)".to_string(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

/// sc.exe outputs in the system ANSI code page; UTF-8 decoding would
/// garble the error messages.
fn decode_console_output(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }

    use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MB_PRECOMPOSED};

    unsafe {
        let len = MultiByteToWideChar(CP_ACP, MB_PRECOMPOSED, bytes, None);
        if len <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; len as usize];
        if MultiByteToWideChar(CP_ACP, MB_PRECOMPOSED, bytes, Some(&mut wide)) <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide)
    }
}

fn is_service_missing(output: &Output) -> bool {
    let text = command_output_text(output);
    text.contains("1060") || text.contains("The specified service does not exist")
}

fn is_access_denied(output: &Output) -> bool {
    output.status.code() == Some(5)
        || command_output_text(output).contains("Access is denied")
}

fn run_elevated_sc(command: &str, description: &str) -> Result<Output, String> {
    let temp_bat = std::env::temp_dir().join(format!("cpum_service_{}_{}.bat", command, Uuid::new_v4()));
    let content = format!("@echo off\r\nsc.exe {command} {SERVICE_NAME}\r\nexit /b %errorlevel%\r\n");
    std::fs::write(&temp_bat, content).map_err(|e| format!("failed to create temp script: {e}"))?;
    let escaped_path = temp_bat.display().to_string().replace('\'', "''");
    let ps_cmd = format!(
        "$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', '{escaped_path}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode"
    );
    let result = run_command_with_timeout(
        Command::new("powershell.exe").args(["-NoProfile", "-Command", &ps_cmd]),
        description,
    );
    let _ = std::fs::remove_file(temp_bat);
    result
}

/// Get the affinity rules directory (passed to the service as a startup arg).
fn rules_dir_string<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<String, String> {
    Ok(rules_dir(app)?.to_string_lossy().into_owned())
}

/// Install cpum_service as a Windows service (auto-start).
/// The app itself runs un-elevated, so the sc.exe calls are relaunched
/// through an elevated helper (which shows a UAC prompt).
#[tauri::command]
fn install_service<R: Runtime>(app: tauri::AppHandle<R>) -> Result<String, String> {
    let svc_path = service_exe_path()?;
    let rules_dir = rules_dir_string(&app)?;

    // sc.exe needs the full command line as the binPath value; the inner
    // double quotes must be escaped, otherwise install paths containing
    // spaces will cause sc.exe to return 1639 (invalid command-line argument).
    let bin_path_value = format!(r#"\"{}\" \"{}\""#, svc_path.display(), rules_dir);

    // Create a temp batch file and capture the elevated sc.exe output so we
    // can report the real error.
    let temp_bat = std::env::temp_dir().join(format!("cpum_install_service_{}.bat", Uuid::new_v4()));
    let temp_log = temp_bat.with_extension("log");
    let escaped_log_path = temp_log.display().to_string().replace('"', "\"");
    let bat_content = format!(
        "@echo off\r\nsc.exe query {SERVICE_NAME} >nul 2>&1\r\nif errorlevel 1 (\r\n  sc.exe create {SERVICE_NAME} binPath= \"{bin_path_value}\" start= auto DisplayName= \"CPU Affinity Manager Service\" > \"{escaped_log_path}\" 2>&1\r\n) else (\r\n  sc.exe config {SERVICE_NAME} binPath= \"{bin_path_value}\" start= auto >> \"{escaped_log_path}\" 2>&1\r\n)\r\nif errorlevel 1 exit /b %errorlevel%\r\nsc.exe description {SERVICE_NAME} \"Automatically applies CPU affinity rules to running processes on boot and process launch.\" >> \"{escaped_log_path}\" 2>&1\r\nif errorlevel 1 exit /b %errorlevel%\r\nsc.exe start {SERVICE_NAME} >> \"{escaped_log_path}\" 2>&1\r\nexit /b %errorlevel%\r\n"
    );
    std::fs::write(&temp_bat, bat_content).map_err(|e| format!("failed to create temp script: {}", e))?;

    // Run via PowerShell with elevation
    let escaped_bat_path = temp_bat.display().to_string().replace('\'', "''");
    let ps_cmd = format!(
        "$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', '{}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        escaped_bat_path
    );
    let output = run_command_with_timeout(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &ps_cmd]),
        "elevated service install",
    )?;

    let script_output = std::fs::read(&temp_log)
        .map(|bytes| decode_console_output(&bytes).trim().to_string())
        .unwrap_or_default();

    // Clean up temp files
    let _ = std::fs::remove_file(&temp_bat);
    let _ = std::fs::remove_file(&temp_log);

    if !output.status.success() {
        let status = run_command_with_timeout(
            Command::new("sc.exe").args(["query", SERVICE_NAME]),
            "post-install service status check",
        )
        .map(|check| command_output_text(&check))
        .unwrap_or_else(|error| error);
        return Err(format!(
            "service install failed (elevated command exit code {:?}): {}\ncurrent service status: {}",
            output.status.code(),
            if script_output.is_empty() {
                command_output_text(&output)
            } else {
                script_output
            },
            status
        ));
    }

    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Verify the service was actually installed
    let check_output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "service install status check",
    )?;

    if is_service_missing(&check_output) {
        return Err(format!("service not found after install: {}", command_output_text(&check_output)));
    }

    Ok("service installed and started. It will auto-run on boot; no need to keep the app open.".to_string())
}

/// Uninstall the cpum_service. The sc.exe calls are relaunched elevated when
/// the un-elevated attempt is refused with access denied.
#[tauri::command]
fn uninstall_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Ok("service is not installed, nothing to uninstall.".to_string());
    }

    // Try to stop the service first (multiple attempts)
    for _ in 0..3 {
        let _ = run_command_with_timeout(
            Command::new("sc.exe").args(["stop", SERVICE_NAME]),
            "stop service",
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Wait for the service to stop completely
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // Delete the service
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["delete", SERVICE_NAME]),
        "delete service",
    )?;

    if !output.status.success() {
        if is_service_missing(&output) {
            return Ok("service is not installed, nothing to uninstall.".to_string());
        }
        if is_access_denied(&output) {
            output = run_elevated_sc("delete", "elevated delete service")?;
        }
        if !output.status.success() {
            return Err(format!("failed to delete service: {}", command_output_text(&output)));
        }
    }

    // Verify the service has been removed
    std::thread::sleep(std::time::Duration::from_millis(500));
    let check_output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "post-uninstall service status check",
    )?;
    if !is_service_missing(&check_output) {
        return Err("service uninstall failed: the service may still be running or locked".to_string());
    }

    Ok("service uninstalled.".to_string())
}

/// Whether the privileged bridge to the LocalSystem service is reachable.
/// Returns "connected" or "unavailable"; when connected, operations on
/// protected / other-session processes work without any UAC prompt.
#[tauri::command]
fn get_bridge_status<R: Runtime>(app: tauri::AppHandle<R>) -> String {
    let Ok(token) = bridge_token(&app) else {
        return "unavailable".to_string();
    };
    match cpum_core::ipc::request(&cpum_core::ipc::Request::Ping { token }) {
        Ok(response) if response.ok => "connected".to_string(),
        _ => "unavailable".to_string(),
    }
}

/// Query the service status. Returns one of:
/// "running" / "stopped" / "not_installed" / "unknown:<state>".
#[tauri::command]
async fn get_service_status() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(query_service_status)
        .await
        .map_err(|e| format!("service status task failed: {e}"))?
}

fn query_service_status() -> Result<String, String> {
    let output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "service status query",
    )?;

    if is_service_missing(&output) {
        return Ok("not_installed".to_string())
    }
    if !output.status.success() {
        return Err(format!("failed to query service status: {}", command_output_text(&output)));
    }

    let stdout = decode_console_output(&output.stdout);

    // STATE field: 1=STOPPED, 2=START_PENDING, 3=STOP_PENDING, 4=RUNNING
    if stdout.contains("RUNNING") {
        Ok("running".to_string())
    } else if stdout.contains("STOPPED") {
        Ok("stopped".to_string())
    } else {
        // Extract the STATE value
        for line in stdout.lines() {
            if line.contains("STATE") {
                return Ok(format!("unknown: {}", line.trim()));
            }
        }
        Err(format!("unrecognized service status: {}", command_output_text(&output)))
    }
}

/// Start the service (when installed but stopped).
#[tauri::command]
fn start_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Err("service is not installed, please install it first.".to_string());
    }
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["start", SERVICE_NAME]),
        "start service",
    )?;

    if !output.status.success() {
        if is_access_denied(&output) {
            output = run_elevated_sc("start", "elevated start service")?;
        }
        if !output.status.success() {
            return Err(format!("failed to start service: {}", command_output_text(&output)));
        }
    }

    Ok("service started.".to_string())
}

/// Stop the service.
#[tauri::command]
fn stop_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Err("service is not installed, please install it first.".to_string());
    }
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["stop", SERVICE_NAME]),
        "stop service",
    )?;

    if !output.status.success() {
        if is_access_denied(&output) {
            output = run_elevated_sc("stop", "elevated stop service")?;
        }
        if !output.status.success() {
            return Err(format!("failed to stop service: {}", command_output_text(&output)));
        }
    }

    Ok("service stopped.".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // Updater plugin: serves the embedded `plugins.updater` config
        // (pubkey + endpoints) to the frontend, which uses
        // `@tauri-apps/plugin-updater` to check and apply releases. Build-time
        // signing is handled in `.github/workflows/release.yml` via
        // `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]`.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_cpu_topology,
            get_logical_processor_usage,
            get_process_exe_path,
            list_processes,
            list_processes_light,
            list_processes_cached,
            set_process_affinity,
            set_process_priority,
            start_metrics_stream,
            stop_metrics_stream,
            load_cpu_topology_cache,
            save_cpu_topology_cache,
            get_probalance_config,
            save_probalance_config,
            get_probalance_status,
            get_probalance_log,
            get_probalance_statistics,
            save_affinity_rules,
            load_affinity_rules,
            add_affinity_rule,
            update_affinity_rule,
            delete_affinity_rule,
            apply_affinity_rules,
            generate_affinity_rule_id,
            install_service,
            uninstall_service,
            get_service_status,
            get_bridge_status,
            start_service,
            stop_service,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}


