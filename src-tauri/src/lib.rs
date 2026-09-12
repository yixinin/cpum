// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
// The single implementation of the rule model / matching / persistence /
// application engine lives in the cpum-core crate (shared by the GUI and the
// Windows service). This module is only a thin Tauri command layer.

mod models;
mod process;
mod topology;

use models::{mask_to_hex, CpuTopology, ProcessInfo};
use cpum_core::rule::{AffinityRule, MatchType, RuleMode};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use tauri::{Emitter, Manager, Runtime};
use uuid::Uuid;

#[tauri::command]
fn get_process_exe_path(pid: u32) -> Option<String> {
    cpum_core::procwin::get_process_exe_path(pid)
}

#[tauri::command]
fn set_process_priority<R: Runtime>(
    app: tauri::AppHandle<R>, pid: u32, priority_class: Option<u32>, io_priority: Option<u32>, memory_priority: Option<u32>,
) -> Result<(), String> {
    if let Some(value) = priority_class { cpum_core::procwin::set_process_priority_class(pid, value)?; }
    if let Some(value) = io_priority { cpum_core::procwin::set_process_io_priority(pid, value)?; }
    if let Some(value) = memory_priority { cpum_core::procwin::set_process_memory_priority(pid, value)?; }
    let current = cpum_core::procwin::get_process_priorities(pid);
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
    let masks = match group_masks {
        Some(values) => cpum_core::procwin::GroupMasks::from_hex_list(&values)?.0,
        None => vec![cpum_core::procwin::parse_hex_mask(&mask)?],
    };
    cpum_core::procwin::set_affinity_by_group_masks(pid, &masks, mode.unwrap_or_default())?;
    // On success, push an event to the frontend immediately so it can patch
    // the corresponding row's mask in place without a full table refresh.
    let new_mask_hex = masks.first().copied().map(mask_to_hex);
    let payload = process::build_affinity_updated_event(pid, new_mask_hex);
    let _ = app.emit("process://affinity-updated", payload);
    Ok(())
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

/// Rules are shared between the elevated desktop app and the LocalSystem
/// service, so they must live in a machine-level directory and cannot use
/// the service account's own APPDATA.
fn machine_rules_dir() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("cpum")
}

#[tauri::command]
fn get_logical_processor_usage() -> Result<Vec<process::LogicalProcessorUsage>, String> {
    process::sample_logical_processor_usage()
}

#[tauri::command]
fn get_probalance_config() -> Result<cpum_core::probalance::ProBalanceConfig, String> {
    cpum_core::probalance::load_config(&machine_rules_dir())
}

#[tauri::command]
fn save_probalance_config(config: cpum_core::probalance::ProBalanceConfig) -> Result<(), String> {
    cpum_core::probalance::save_config(&machine_rules_dir(), &config)
}

#[tauri::command]
fn get_probalance_status() -> Option<cpum_core::probalance::PbStatus> {
    cpum_core::probalance::read_status(&machine_rules_dir())
}

#[tauri::command]
fn get_probalance_log(limit: usize) -> Vec<cpum_core::probalance::PbLogEntry> {
    cpum_core::probalance::read_log(&machine_rules_dir(), limit.min(500))
}

#[tauri::command]
fn get_probalance_statistics() -> cpum_core::probalance::PbStatistics {
    cpum_core::probalance::statistics(&machine_rules_dir())
}

fn app_rules_dir<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("failed to get app_data_dir: {e}"))
}

fn legacy_rules_dir() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|appdata| PathBuf::from(appdata).join("cpum"))
}

/// Save the affinity rule list.
#[tauri::command]
fn save_affinity_rules<R: Runtime>(
    _app: tauri::AppHandle<R>,
    rules: Vec<AffinityRule>,
) -> Result<(), String> {
    for rule in &rules { cpum_core::rule::validate_rule(rule)?; }
    cpum_core::store::save_rules(&machine_rules_dir(), &rules)
}

/// Load the affinity rule list.
#[tauri::command]
fn load_affinity_rules<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<AffinityRule>, String> {
    let rules_dir = machine_rules_dir();
    let primary_path = affinity_rules_path(&rules_dir);
    let (path, raw) = match std::fs::read_to_string(&primary_path) {
        Ok(raw) => (primary_path, raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let app_dir = app_rules_dir(&app)?;
            let mut legacy_paths = vec![affinity_rules_path(&app_dir)];
            if let Some(legacy_dir) = legacy_rules_dir() {
                legacy_paths.push(affinity_rules_path(&legacy_dir));
            }

            let mut found = None;
            for legacy_path in legacy_paths {
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

    // When rules are first read from a legacy location, migrate them to the
    // machine-level directory that the service can read directly.
    if path != affinity_rules_path(&rules_dir) {
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
    for changed in report.changed {
        let _ = app.emit("process://affinity-updated", process::build_affinity_updated_event(changed.pid, changed.mask_hex));
        if let Some(priorities) = changed.priorities {
            let _ = app.emit("process://priority-updated", serde_json::json!({ "pid": changed.pid, "priority_class": priorities.priority_class, "io_priority": priorities.io_priority, "memory_priority": priorities.memory_priority }));
        }
    }
    Ok(report.applied)
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
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run_command_with_timeout(command: &mut Command, description: &str) -> Result<Output, String> {
    command.creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start {description}: {e}"))?;
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;

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
            return Err(format!("{description} timed out ({} seconds)", COMMAND_TIMEOUT.as_secs()));
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
fn rules_dir_string() -> Result<String, String> {
    let dir = machine_rules_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create rules directory: {e}"))?;
    }
    Ok(dir.to_string_lossy().into_owned())
}

/// Install cpum_service as a Windows service (auto-start).
/// Requires administrator privileges (the app is already running elevated).
#[tauri::command]
fn install_service<R: Runtime>(_app: tauri::AppHandle<R>) -> Result<String, String> {
    let svc_path = service_exe_path()?;
    let rules_dir = rules_dir_string()?;

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

/// Uninstall the cpum_service.
/// Requires administrator privileges (the app is already running elevated).
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
            start_service,
            stop_service,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}


