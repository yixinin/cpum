// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

mod core;
mod models;
mod process;
mod topology;

use models::{mask_to_hex, AffinityRule, CpuTopology, ProcessInfo};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{Emitter, Manager, Runtime};
use uuid::Uuid;

/// 返回当前系统的 CPU 拓扑 (逻辑处理器 / 物理核 / CCD)
#[tauri::command]
fn get_cpu_topology() -> Result<CpuTopology, String> {
    topology::get_cpu_topology()
}

/// 枚举所有进程及其当前 CPU 亲和性 (首屏全量加载时调用, 也写入 metrics 采样 baseline)
/// 完成后自动写入进程列表缓存, 下次启动 <1ms 读取
#[tauri::command]
fn list_processes<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<ProcessInfo>, String> {
    let result = process::list_processes()?;
    // 异步写入缓存 (失败静默忽略)
    if let Ok(cache_dir) = app.path().app_data_dir() {
        process::save_processes_cache(&cache_dir, &result);
    }
    Ok(result)
}

/// 超轻量快扫 (只含 PID / name / parent_pid), <20ms 返回, 用于首屏立即出内容
#[tauri::command]
fn list_processes_light() -> Result<Vec<ProcessInfo>, String> {
    process::list_processes_light()
}

/// 读取上次保存的进程列表缓存 (<1ms)。首次启动无缓存返回空 Vec。
#[tauri::command]
fn list_processes_cached<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Vec<ProcessInfo>, String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("取 app_data_dir 失败: {e}"))?;
    Ok(process::load_processes_cache(&cache_dir).unwrap_or_default())
}

/// 设置指定进程的 CPU 亲和性 mask
/// mask 以十六进制字符串传入 (如"0xFF"), 以兼容 64 位全 1 的情况
#[tauri::command]
fn set_process_affinity<R: Runtime>(
    app: tauri::AppHandle<R>,
    pid: u32,
    mask: String,
) -> Result<(), String> {
    let parsed = process::parse_hex_mask(&mask)?;
    process::set_process_affinity(pid, parsed)?;
    // 成功后立刻推一条事件给前端, 前端就地 patch 对应行的 mask, 不需要再整表全量刷新
    let new_mask_hex = Some(mask_to_hex(parsed));
    let payload = process::build_affinity_updated_event(pid, new_mask_hex);
    let _ = app.emit("process://affinity-updated", payload);
    Ok(())
}

// ---------- 指标分步推送流 ----------

/// 启动后端指标推送流 (每秒 1 次采样, 分 4 波 + 结构 diff 推事件, 避免前端整表闪烁)
#[tauri::command]
fn start_metrics_stream<R: Runtime>(
    app: tauri::AppHandle<R>,
    interval_ms: u32,
) -> Result<(), String> {
    let app_clone = app.clone();
    process::start_metrics_stream_in_thread(interval_ms, move |event, payload| {
        app_clone
            .emit(event, payload)
            .map_err(|e| format!("emit 失败: {}", e))
    })
}

/// 停止后端指标推送流 (暂停)
#[tauri::command]
fn stop_metrics_stream() -> Result<(), String> {
    process::stop_metrics_stream_in_thread()
}

// ---------- CPU 拓扑缓存 (加速启动, CPU 拓扑几乎永远不变) ----------

/// 读缓存: 先做 <1ms 的硬件签名 (vendor+family+model+LP 数), 签名匹配才返回缓存
/// 否则返回 None, 前端就回退到真实探测。签名不匹配的场景: CPU/主板被更换。
#[tauri::command]
fn load_cpu_topology_cache<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<topology::TopologyCache>, String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("取 app_data_dir 失败: {e}"))?;
    // 先通过 GetSystemInfo 快速拿 LP 数 (不做 CPUID pinning)
    let total_lps: u32 = topology::sys_info_logical_processor_count()
        .unwrap_or(0);
    let signature = topology::hw_signature_fast(total_lps);
    topology::load_topology_cache(&cache_dir, &signature)
}

/// 写缓存: 拓扑后端全量探测完之后调, 下次启动就能读缓存 (<1ms vs 1-2s)
#[tauri::command]
fn save_cpu_topology_cache<R: Runtime>(
    app: tauri::AppHandle<R>,
    topology: CpuTopology,
) -> Result<(), String> {
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("取 app_data_dir 失败: {e}"))?;
    let total_lps = topology.total_logical_processors;
    let signature = topology::hw_signature_fast(total_lps);
    topology::save_topology_cache(&cache_dir, &topology, &signature)
}

// ---------- 亲和性规则持久化 ----------

fn affinity_rules_path(base_dir: &PathBuf) -> PathBuf {
    base_dir.join("affinity_rules.json")
}

/// 规则由提升权限的桌面应用和 LocalSystem 服务共同使用，必须放在机器级目录，
/// 不能使用服务账户自己的 APPDATA。
fn machine_rules_dir() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("cpum")
}

fn app_rules_dir<R: Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("取 app_data_dir 失败: {e}"))
}

fn legacy_rules_dir() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|appdata| PathBuf::from(appdata).join("cpum"))
}

/// 保存亲和性规则列表
#[tauri::command]
fn save_affinity_rules<R: Runtime>(
    _app: tauri::AppHandle<R>,
    rules: Vec<AffinityRule>,
) -> Result<(), String> {
    let rules_dir = machine_rules_dir();

    // 确保目录存在
    if !rules_dir.exists() {
        std::fs::create_dir_all(&rules_dir)
            .map_err(|e| format!("创建缓存目录失败: {e}"))?;
    }

    let path = affinity_rules_path(&rules_dir);
    let json = serde_json::to_string_pretty(&rules)
        .map_err(|e| format!("序列化规则失败: {e}"))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("写入规则文件失败: {e}"))?;

    Ok(())
}

/// 加载亲和性规则列表
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
                    Err(error) => return Err(format!("读取规则文件失败: {error}")),
                }
            }
            let Some(found) = found else {
                return Ok(vec![]);
            };
            found
        }
        Err(error) => return Err(format!("读取规则文件失败: {error}")),
    };
    let rules: Vec<AffinityRule> = serde_json::from_str(&raw)
        .map_err(|e| format!("解析规则文件失败: {e}"))?;

    // 首次读取旧位置的规则时迁移到服务可直接读取的机器级目录。
    if path != affinity_rules_path(&rules_dir) {
        save_affinity_rules(app, rules.clone())?;
    }

    Ok(rules)
}

/// 添加一条亲和性规则
#[tauri::command]
fn add_affinity_rule<R: Runtime>(
    app: tauri::AppHandle<R>,
    process_name: String,
    mask: String,
    note: String,
) -> Result<AffinityRule, String> {
    let mut rules = load_affinity_rules(app.clone())?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let rule = AffinityRule {
        id: Uuid::new_v4().to_string(),
        process_name,
        mask,
        enabled: true,
        created_at: now,
        note,
    };

    rules.push(rule.clone());
    save_affinity_rules(app, rules)?;

    Ok(rule)
}

/// 更新一条亲和性规则
#[tauri::command]
fn update_affinity_rule<R: Runtime>(
    app: tauri::AppHandle<R>,
    id: String,
    process_name: Option<String>,
    mask: Option<String>,
    enabled: Option<bool>,
    note: Option<String>,
) -> Result<AffinityRule, String> {
    let mut rules = load_affinity_rules(app.clone())?;

    let rule = rules.iter_mut().find(|r| r.id == id)
        .ok_or_else(|| format!("未找到规则: {}", id))?;

    if let Some(name) = process_name {
        rule.process_name = name;
    }
    if let Some(m) = mask {
        rule.mask = m;
    }
    if let Some(e) = enabled {
        rule.enabled = e;
    }
    if let Some(n) = note {
        rule.note = n;
    }

    let updated = rule.clone();
    save_affinity_rules(app, rules)?;

    Ok(updated)
}

/// 删除一条亲和性规则
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

/// 应用所有启用的亲和性规则到当前运行的进程
/// 返回成功应用的进程数量
#[tauri::command]
fn apply_affinity_rules<R: Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<u32, String> {
    let rules = load_affinity_rules(app.clone())?;
    let processes = process::list_processes()?;
    let mut applied_count = 0;

    for rule in &rules {
        if !rule.enabled {
            continue;
        }

        let mask = process::parse_hex_mask(&rule.mask)?;

        for p in &processes {
            // 匹配进程名 (不区分大小写, 去掉.exe后缀)
            let p_name = p.name.to_lowercase();
            let rule_name = rule.process_name.to_lowercase();
            let matches = p_name == rule_name
                || p_name == format!("{}.exe", rule_name)
                || p_name.trim_end_matches(".exe") == rule_name;

            if matches {
                match process::set_process_affinity(p.pid, mask) {
                    Ok(_) => {
                        applied_count += 1;
                        // 发送事件通知前端
                        let new_mask_hex = Some(mask_to_hex(mask));
                        let payload = process::build_affinity_updated_event(p.pid, new_mask_hex);
                        let _ = app.emit("process://affinity-updated", payload);
                    }
                    Err(e) => {
                        // 单个进程失败不影响其他进程
                        eprintln!("设置进程 {} (PID {}) 亲和性失败: {}", p.name, p.pid, e);
                    }
                }
            }
        }
    }

    Ok(applied_count)
}

/// 自动生成唯一ID的亲和性规则
#[tauri::command]
fn generate_affinity_rule_id() -> String {
    Uuid::new_v4().to_string()
}

// ==========================================================================
// Windows 服务管理 (cpum_service.exe)
// ==========================================================================

/// 获取随安装包发布的服务可执行体路径。
/// Tauri 会将 bundle.resources 放到 Windows 安装目录的 resources 子目录。
fn service_exe_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("取当前 exe 路径失败: {e}"))?;
    let dir = exe.parent().ok_or("无法确定 exe 所在目录")?;
    let candidates = [
        dir.join("resources").join("cpum_service.exe"),
        // 兼容开发环境及早期手动部署方式。
        dir.join("cpum_service.exe"),
    ];

    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "服务程序不存在。请重新安装包含 cpum_service.exe 的版本（已检查: {}）",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、")
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
        .map_err(|e| format!("启动 {description} 失败: {e}"))?;
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;

    loop {
        if child
            .try_wait()
            .map_err(|e| format!("等待 {description} 失败: {e}"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|e| format!("读取 {description} 输出失败: {e}"));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{description} 超时（{} 秒）", COMMAND_TIMEOUT.as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn command_output_text(output: &Output) -> String {
    let stdout = decode_console_output(&output.stdout).trim().to_string();
    let stderr = decode_console_output(&output.stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "（命令未返回详细信息）".to_string(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

/// sc.exe 按系统 ANSI 代码页输出中文；UTF-8 解码会使错误信息乱码。
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
    text.contains("1060") || text.contains("指定的服务未安装")
}

fn is_access_denied(output: &Output) -> bool {
    output.status.code() == Some(5)
        || command_output_text(output).contains("拒绝访问")
        || command_output_text(output).contains("Access is denied")
}

fn run_elevated_sc(command: &str, description: &str) -> Result<Output, String> {
    let temp_bat = std::env::temp_dir().join(format!("cpum_service_{}_{}.bat", command, Uuid::new_v4()));
    let content = format!("@echo off\r\nsc.exe {command} {SERVICE_NAME}\r\nexit /b %errorlevel%\r\n");
    std::fs::write(&temp_bat, content).map_err(|e| format!("创建临时脚本失败: {e}"))?;
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

/// 获取亲和性规则文件所在目录（传递给服务作为启动参数）
fn rules_dir_string() -> Result<String, String> {
    let dir = machine_rules_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("创建规则目录失败: {e}"))?;
    }
    Ok(dir.to_string_lossy().into_owned())
}

/// /// 安装 cpum_service 为 Windows 服务（自动启动）
/// 需要管理员权限（应用已以管理员身份运行）
#[tauri::command]
fn install_service<R: Runtime>(_app: tauri::AppHandle<R>) -> Result<String, String> {
    let svc_path = service_exe_path()?;
    let rules_dir = rules_dir_string()?;

    // sc.exe 需要把完整命令行作为 binPath 值传入；内部的双引号必须转义，
    // 否则带空格的安装目录会使 sc.exe 返回 1639（命令行参数无效）。
    let bin_path_value = format!(r#"\"{}\" \"{}\""#, svc_path.display(), rules_dir);

    // 创建临时批处理文件，并保存提升后 sc.exe 的输出以便报告实际错误。
    let temp_bat = std::env::temp_dir().join(format!("cpum_install_service_{}.bat", Uuid::new_v4()));
    let temp_log = temp_bat.with_extension("log");
    let escaped_log_path = temp_log.display().to_string().replace('"', "\"");
    let bat_content = format!(
        "@echo off\r\nsc.exe query {SERVICE_NAME} >nul 2>&1\r\nif errorlevel 1 (\r\n  sc.exe create {SERVICE_NAME} binPath= \"{bin_path_value}\" start= auto DisplayName= \"CPU Affinity Manager Service\" > \"{escaped_log_path}\" 2>&1\r\n) else (\r\n  sc.exe config {SERVICE_NAME} binPath= \"{bin_path_value}\" start= auto >> \"{escaped_log_path}\" 2>&1\r\n)\r\nif errorlevel 1 exit /b %errorlevel%\r\nsc.exe description {SERVICE_NAME} \"Automatically applies CPU affinity rules to running processes on boot and process launch.\" >> \"{escaped_log_path}\" 2>&1\r\nif errorlevel 1 exit /b %errorlevel%\r\nsc.exe start {SERVICE_NAME} >> \"{escaped_log_path}\" 2>&1\r\nexit /b %errorlevel%\r\n"
    );
    std::fs::write(&temp_bat, bat_content).map_err(|e| format!("创建临时脚本失败: {}", e))?;

    // 使用 PowerShell 以管理员权限运行
    let escaped_bat_path = temp_bat.display().to_string().replace('\'', "''");
    let ps_cmd = format!(
        "$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', '{}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        escaped_bat_path
    );
    let output = run_command_with_timeout(
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &ps_cmd]),
        "管理员服务安装",
    )?;

    let script_output = std::fs::read(&temp_log)
        .map(|bytes| decode_console_output(&bytes).trim().to_string())
        .unwrap_or_default();

    // 清理临时文件
    let _ = std::fs::remove_file(&temp_bat);
    let _ = std::fs::remove_file(&temp_log);

    if !output.status.success() {
        let status = run_command_with_timeout(
            Command::new("sc.exe").args(["query", SERVICE_NAME]),
            "服务安装失败后的状态检查",
        )
        .map(|check| command_output_text(&check))
        .unwrap_or_else(|error| error);
        return Err(format!(
            "服务安装失败（管理员命令退出码 {:?}）：{}\n当前服务状态：{}",
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

    // 检查服务是否安装成功
    let check_output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "服务安装状态检查",
    )?;

    if is_service_missing(&check_output) {
        return Err(format!("服务安装后未找到服务：{}", command_output_text(&check_output)));
    }

    Ok("服务安装并启动成功。开机后将自动运行，无需手动打开应用。".to_string())
}

/// 卸载 cpum_service 服务
/// 需要管理员权限（应用已以管理员身份运行）
#[tauri::command]
fn uninstall_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Ok("服务未安装，无需卸载。".to_string());
    }

    // 先停止服务（多次尝试）
    for _ in 0..3 {
        let _ = run_command_with_timeout(
            Command::new("sc.exe").args(["stop", SERVICE_NAME]),
            "停止服务",
        );
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // 等待服务完全停止
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // 删除服务
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["delete", SERVICE_NAME]),
        "删除服务",
    )?;

    if !output.status.success() {
        if is_service_missing(&output) {
            return Ok("服务未安装，无需卸载。".to_string());
        }
        if is_access_denied(&output) {
            output = run_elevated_sc("delete", "管理员删除服务")?;
        }
        if !output.status.success() {
            return Err(format!("删除服务失败：{}", command_output_text(&output)));
        }
    }

    // 检查服务是否已卸载
    std::thread::sleep(std::time::Duration::from_millis(500));
    let check_output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "服务卸载状态检查",
    )?;
    if !is_service_missing(&check_output) {
        return Err("服务卸载失败：服务可能仍在运行或被锁定".to_string());
    }

    Ok("服务已卸载。".to_string())
}

/// 查询服务状态：返回 "running" / "stopped" / "not_installed" / "unknown:<state>"
#[tauri::command]
async fn get_service_status() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(query_service_status)
        .await
        .map_err(|e| format!("查询服务状态任务失败: {e}"))?
}

fn query_service_status() -> Result<String, String> {
    let output = run_command_with_timeout(
        Command::new("sc.exe").args(["query", SERVICE_NAME]),
        "服务状态查询",
    )?;

    if is_service_missing(&output) {
        return Ok("not_installed".to_string())
    }
    if !output.status.success() {
        return Err(format!("查询服务状态失败：{}", command_output_text(&output)));
    }

    let stdout = decode_console_output(&output.stdout);

    // STATE 字段: 1=STOPPED, 2=START_PENDING, 3=STOP_PENDING, 4=RUNNING
    if stdout.contains("RUNNING") {
        Ok("running".to_string())
    } else if stdout.contains("STOPPED") {
        Ok("stopped".to_string())
    } else {
        // 提取 STATE 数值
        for line in stdout.lines() {
            if line.contains("STATE") {
                return Ok(format!("unknown: {}", line.trim()));
            }
        }
        Err(format!("无法识别服务状态：{}", command_output_text(&output)))
    }
}

/// 启动服务（已安装但停止时）
#[tauri::command]
fn start_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Err("服务未安装，请先安装服务。".to_string());
    }
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["start", SERVICE_NAME]),
        "启动服务",
    )?;

    if !output.status.success() {
        if is_access_denied(&output) {
            output = run_elevated_sc("start", "管理员启动服务")?;
        }
        if !output.status.success() {
            return Err(format!("启动服务失败：{}", command_output_text(&output)));
        }
    }

    Ok("服务已启动。".to_string())
}

/// 停止服务
#[tauri::command]
fn stop_service() -> Result<String, String> {
    if query_service_status()? == "not_installed" {
        return Err("服务未安装，请先安装服务。".to_string());
    }
    let mut output = run_command_with_timeout(
        Command::new("sc.exe").args(["stop", SERVICE_NAME]),
        "停止服务",
    )?;

    if !output.status.success() {
        if is_access_denied(&output) {
            output = run_elevated_sc("stop", "管理员停止服务")?;
        }
        if !output.status.success() {
            return Err(format!("停止服务失败：{}", command_output_text(&output)));
        }
    }

    Ok("服务已停止。".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_cpu_topology,
            list_processes,
            list_processes_light,
            list_processes_cached,
            set_process_affinity,
            start_metrics_stream,
            stop_metrics_stream,
            load_cpu_topology_cache,
            save_cpu_topology_cache,
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


