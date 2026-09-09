//! Windows 服务：CPU 亲和性规则守护进程
//!
//! 安装后以 SYSTEM 身份运行，每 5 秒扫描一次进程并自动应用亲和性规则。
//! 规则文件路径通过服务启动参数传入（安装时由 Tauri 命令自动设置）。

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode,
    ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::Result as WinSvcResult;

const SERVICE_NAME: &str = "CpumAffinityService";
const RULES_DIR: &str = r"C:\ProgramData\cpum";

/// 轮询间隔（秒）
const POLL_INTERVAL_SECS: u64 = 5;

// ---------- 规则模型 (与 cpum_lib::models::AffinityRule 保持一致) ----------

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone, Debug)]
struct AffinityRule {
    #[allow(dead_code)]
    id: String,
    process_name: String,
    mask: String,
    enabled: bool,
    #[allow(dead_code)]
    created_at: u64,
    #[allow(dead_code)]
    note: String,
}

// ---------- 规则文件 I/O ----------

fn load_rules(base_dir: &Path) -> Result<Vec<AffinityRule>, String> {
    let path = base_dir.join("affinity_rules.json");
    if !path.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取规则文件: {e}"))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("解析规则文件: {e}"))
}

fn parse_hex_mask(s: &str) -> Result<u64, String> {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.is_empty() {
        return Err("mask 为空".into());
    }
    if trimmed.len() > 16 {
        return Err("mask 超过 64 位".into());
    }
    u64::from_str_radix(trimmed, 16).map_err(|e| format!("解析 mask: {e}"))
}

fn name_matches(process_name: &str, rule_name: &str) -> bool {
    let p = process_name.to_lowercase();
    let r = rule_name.to_lowercase();
    p == r || p == format!("{}.exe", r) || p.trim_end_matches(".exe") == r
}

// ---------- Win32 进程枚举（轻量版，无指标采集）----------

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, SetLastError, LUID, WIN32_ERROR, ERROR_NOT_ALL_ASSIGNED};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY, SE_PRIVILEGE_ENABLED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, SetProcessAffinityMask,
    PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION,
};

struct SimpleProcess {
    pid: u32,
    name: String,
}

/// LocalSystem 持有 SeDebugPrivilege，但默认可能处于禁用状态。启用后才能
/// 稳定地打开交互用户会话中的进程并修改亲和性。
fn enable_debug_privilege() -> Result<(), String> {
    unsafe {
        let mut token = Default::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

        let mut luid = LUID::default();
        let lookup_result = LookupPrivilegeValueW(None, w!("SeDebugPrivilege"), &mut luid);
        if let Err(error) = lookup_result {
            let _ = CloseHandle(token);
            return Err(format!("LookupPrivilegeValueW(SeDebugPrivilege): {error}"));
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(WIN32_ERROR(0));
        let adjust_result = AdjustTokenPrivileges(
            token,
            false,
            Some(&privileges),
            0,
            None,
            None,
        );
        let last_error = GetLastError();
        let _ = CloseHandle(token);

        adjust_result.map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
        if last_error == ERROR_NOT_ALL_ASSIGNED {
            return Err("当前服务账户不具备 SeDebugPrivilege".to_string());
        }
    }
    Ok(())
}

fn enumerate_processes() -> Result<Vec<SimpleProcess>, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot: {e}"))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(
                    &entry.szExeFile[..entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)],
                );
                processes.push(SimpleProcess {
                    pid: entry.th32ProcessID,
                    name,
                });
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        Ok(processes)
    }
}

fn set_affinity(pid: u32, mask: u64) -> Result<(), String> {
    unsafe {
        let handle = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess(PID {}): {}", pid, e))?;
        let result = SetProcessAffinityMask(handle, mask as usize);
        let _ = CloseHandle(handle);
        result.map_err(|e| format!("SetProcessAffinityMask(PID {}): {}", pid, e))
    }
}

/// 应用所有启用的规则，返回 (成功数, 失败数)
fn apply_all_rules(base_dir: &Path) -> Result<(u32, u32), String> {
    let rules = load_rules(base_dir)?;
    let processes = enumerate_processes()?;
    let mut ok = 0u32;
    let mut fail = 0u32;

    for rule in &rules {
        if !rule.enabled {
            continue;
        }
        let mask = match parse_hex_mask(&rule.mask) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for p in &processes {
            if name_matches(&p.name, &rule.process_name) {
                match set_affinity(p.pid, mask) {
                    Ok(_) => ok += 1,
                    Err(_) => fail += 1,
                }
            }
        }
    }
    Ok((ok, fail))
}

// ---------- Windows Service 主循环 ----------

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    // ServiceMain 的 argv[0] 是服务名；安装命令写入的规则目录从 argv[1] 开始。
    let rules_dir = arguments
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(RULES_DIR));

    if let Err(e) = run_service(rules_dir) {
        eprintln!("Service fatal: {e}");
    }
}

fn run_service(rules_dir: PathBuf) -> WinSvcResult<()> {
    enable_debug_privilege()
        .map_err(|message| windows_service::Error::Winapi(std::io::Error::other(message)))?;
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

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

    // 主循环：每 POLL_INTERVAL_SECS 秒扫描一次
    loop {
        let _ = apply_all_rules(&rules_dir);
        if shutdown_rx.recv_timeout(Duration::from_secs(POLL_INTERVAL_SECS)).is_ok() {
            break;
        }
    }

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

// ---------- 入口 ----------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    // --apply-once <dir>  测试模式：执行一次后退出
    if args.get(1).map(|s| s.as_str()) == Some("--apply-once") {
        let dir = args
            .get(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(RULES_DIR));
        let (ok, fail) = apply_all_rules(&dir)?;
        println!("Applied: {ok} ok, {fail} failed");
        return Ok(());
    }

    // 正常模式：作为 Windows 服务运行
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}
