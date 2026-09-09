//! 进程枚举、CPU 亲和性读写与资源使用率 (CPU / Mem / Disk / Net) 采样
//!
//! 资源使用率设计:
//!  - CPU / Disk / Net 都是 "差值 / 时间间隔" 类型指标, 必须保留上一次快照才能计算速率。
//!  - 用进程级全局 Mutex 缓存 `(采样时间, PID -> { CPU 总时间, Disk 读写字节, IO 读写字节 })`。
//!  - 采样间隔 < 250ms 时直接复用缓存的速率值, 避免出现 0% / 尖刺。
//!
//! Win32 API 使用:
//!  - CPU 时间: `GetProcessTimes` (kernel+user 以 100-nanosecond ticks 计)
//!  - 内存 Working Set: `K32GetProcessMemoryInfo` (PROCESS_MEMORY_COUNTERS_EX)
//!  - 磁盘 IO 字节: `GetProcessIoCounters` (ReadTransferCount / WriteTransferCount = 进程累计
//!    IO 字节, 含 disk + net + 命名管道)。Windows 没有公开的"纯 disk 字节"API,
//!    所以「磁盘」列显示总 IO 字节近似。
//!  - 网络 IO 字节: `NtQueryInformationProcess(ProcessNetworkIoCounters=114)`
//!    Win11 24H2+ 原生支持, 返回 PROCESS_NETWORK_COUNTERS { BytesIn, BytesOut }。
//!    老版本 Windows 调用会失败, 网络列显示 0。
//!  - 单调时间: `std::time::Instant` (避免 GetTickCount64 的命名空间不稳定问题)

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;

use once_cell::sync::Lazy;
use serde::Serialize;

use windows::core::{w, PCSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WIN32_ERROR};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{
    GetProcessAffinityMask, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
    OpenProcess, SetProcessAffinityMask,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION,
};

use crate::models::{mask_to_hex, ProcessInfo};

// ---------- ProcessNetworkIoCounters (PROCESS_INFORMATION_CLASS=114, Win11 24H2+) ----------
// windows crate 没有暴露这个 info class, 通过 GetProcAddress 调 ntdll!NtQueryInformationProcess,
// information_class=114 返回 PROCESS_NETWORK_COUNTERS { BytesIn, BytesOut }。
// 老版本 Windows (Win10 / 24H2 之前) 调用会返回非零 NTSTATUS, 此时 net 字段为 0。

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ProcessNetworkCounters {
    /// 进程累计接收字节 (download 方向)
    bytes_in: u64,
    /// 进程累计发送字节 (upload 方向)
    bytes_out: u64,
}

const PROCESS_NETWORK_IO_COUNTERS_CLASS: u32 = 114;

#[allow(non_snake_case)]
type NtQueryInformationProcessFn = unsafe extern "system" fn(
    ProcessHandle: HANDLE,
    ProcessInformationClass: u32,
    ProcessInformation: *mut std::ffi::c_void,
    ProcessInformationLength: u32,
    ReturnLength: *mut u32,
) -> i32; // NTSTATUS

fn resolve_nt_query_info_process() -> Option<NtQueryInformationProcessFn> {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let proc_name: &[u8] = b"NtQueryInformationProcess\0";
        let addr = GetProcAddress(ntdll, PCSTR(proc_name.as_ptr()))?;
        Some(std::mem::transmute::<*const u8, NtQueryInformationProcessFn>(addr as *const u8))
    }
}

static NT_QUERY_PROCESS: Lazy<Option<NtQueryInformationProcessFn>> =
    Lazy::new(resolve_nt_query_info_process);

// ---------- 采样缓存 ----------

#[derive(Clone, Copy, Debug, Default)]
struct ProcessSnapshot {
    cpu_total_ticks: u64,
    disk_read_bytes: u64,
    disk_write_bytes: u64,
    io_read_bytes: u64,
    io_write_bytes: u64,
    net_in_bytes: u64,
    net_out_bytes: u64,
}

static SNAPSHOT_CACHE: Lazy<Mutex<Option<(Instant, HashMap<u32, ProcessSnapshot>)>>> =
    Lazy::new(|| Mutex::new(None));

static RATE_CACHE: Lazy<Mutex<HashMap<u32, RateSample>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, Default)]
struct RateSample {
    cpu_percent: f32,
    disk_read_bps: u64,
    disk_write_bps: u64,
    /// 网络下载速率 (BytesIn delta / dt), bytes/sec
    net_in_bps: u64,
    /// 网络上传速率 (BytesOut delta / dt), bytes/sec
    net_out_bps: u64,
}

const MIN_SAMPLE_INTERVAL_MS: u128 = 250;

// ---------- 指标读取辅助 ----------

fn ft_to_u64(ft: FILETIME) -> u64 {
    unsafe {
        ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64 & 0xFFFF_FFFFu64)
    }
}

fn get_number_of_processors() -> u32 {
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        si.dwNumberOfProcessors
    }
}

fn open_handle(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<HANDLE> {
    unsafe { OpenProcess(access, false, pid).ok() }
}

fn open_handle_for_stats(pid: u32) -> (Option<HANDLE>, bool) {
    // 优先 PROCESS_QUERY_INFORMATION (NtQueryInformationProcess / GetProcessTimes 都需要)
    if let Some(h) = open_handle(pid, PROCESS_QUERY_INFORMATION) {
        return (Some(h), false);
    }
    // 权限不足时退化为 LIMITED (仍可能读到部分计数, 但 NtQuery Disk 会失败)
    match open_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Some(h) => (Some(h), true),
        None => (None, true),
    }
}

fn close_handle(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

struct MetricsRaw {
    cpu_total_ticks: Option<u64>,
    working_set_bytes: u64,
    disk_read_bytes: Option<u64>,
    disk_write_bytes: Option<u64>,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    /// 进程累计接收字节 (BytesIn from ProcessNetworkIoCounters, Win11 24H2+)
    net_in_bytes: Option<u64>,
    /// 进程累计发送字节 (BytesOut from ProcessNetworkIoCounters, Win11 24H2+)
    net_out_bytes: Option<u64>,
}

fn read_metrics_for_handle(handle: HANDLE) -> MetricsRaw {
    unsafe {
        // 1. CPU 时间 (kernel + user, 100-ns ticks)
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let cpu_total = if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok() {
            Some(ft_to_u64(kernel).saturating_add(ft_to_u64(user)))
        } else {
            None
        };

        // 2. 内存 Working Set Size
        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        let ws_bytes = if K32GetProcessMemoryInfo(
            handle,
            &mut pmc as *mut _ as *mut _,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ).as_bool() {
            pmc.WorkingSetSize as u64
        } else {
            0
        };

        // 3. IO 计数器 (GetProcessIoCounters / NtQueryInformationProcess(ProcessIoCounters=2))
        //    返回 ReadTransferCount / WriteTransferCount = 进程累计 IO 字节数 (含 disk + net + 命名管道)。
        //    Windows 没有公开的"纯 disk 字节"计数 API, 用总 IO 字节近似 disk。
        let mut ioc: IO_COUNTERS = std::mem::zeroed();
        let io_rw: Option<(u64, u64)> = if GetProcessIoCounters(handle, &mut ioc).is_ok() {
            Some((ioc.ReadTransferCount, ioc.WriteTransferCount))
        } else {
            None
        };

        // 4. 网络 IO 字节 (Win11 24H2+ 才支持, 老版本调用会失败, 返回 None)
        let net_io: Option<(u64, u64)> = match *NT_QUERY_PROCESS {
            None => None,
            Some(nt_fn) => {
                let mut pnc: ProcessNetworkCounters = std::mem::zeroed();
                let mut bytes_returned: u32 = 0;
                let status = nt_fn(
                    handle,
                    PROCESS_NETWORK_IO_COUNTERS_CLASS,
                    &mut pnc as *mut _ as *mut std::ffi::c_void,
                    size_of::<ProcessNetworkCounters>() as u32,
                    &mut bytes_returned,
                );
                if status == 0 /* STATUS_SUCCESS */
                    && bytes_returned as usize >= size_of::<ProcessNetworkCounters>()
                {
                    Some((pnc.bytes_in, pnc.bytes_out))
                } else {
                    None
                }
            }
        };

        MetricsRaw {
            cpu_total_ticks: cpu_total,
            working_set_bytes: ws_bytes,
            disk_read_bytes: io_rw.map(|x| x.0),
            disk_write_bytes: io_rw.map(|x| x.1),
            io_read_bytes: io_rw.map(|x| x.0),
            io_write_bytes: io_rw.map(|x| x.1),
            net_in_bytes: net_io.map(|x| x.0),
            net_out_bytes: net_io.map(|x| x.1),
        }
    }
}

// ---------- 主流程 ----------

/// 超轻量进程列表: 只含 PID / name / parent_pid, 不做任何 OpenProcess。
/// <20ms 返回, 用于「首屏先立即出内容」, 慢字段 (内存/亲和性/速率) 后续 patch。
pub fn list_processes_light() -> Result<Vec<ProcessInfo>, String> {
    let raw = list_basic_entries()?;
    let mut out: Vec<ProcessInfo> = Vec::with_capacity(raw.len());
    for r in raw {
        out.push(ProcessInfo {
            pid: r.pid,
            name: r.name,
            affinity_mask: None,
            system_affinity_mask: None,
            parent_pid: r.parent_pid,
            access_denied: false, // 未知先假设有权限, 完整版 patch 会覆盖
            cpu_usage_percent: 0.0,
            memory_bytes: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            net_in_bps: 0,
            net_out_bps: 0,
        });
    }
    // 稳定排序: 系统进程在前, 其余按 PID 升序 (给用户一个不突兀的初始顺序)
    out.sort_by(|a, b| {
        let a_sys = is_system_process(a.pid);
        let b_sys = is_system_process(b.pid);
        match (a_sys, b_sys) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.pid.cmp(&b.pid),
        }
    });
    Ok(out)
}

pub fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let now = Instant::now();
    let nproc = get_number_of_processors() as f32;

    // 1. 拿上一次缓存 (可能是 None = 首次调用)
    let prev_opt: Option<(Instant, HashMap<u32, ProcessSnapshot>)> = {
        let guard = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        guard.clone()
    };
    let (prev_time, prev_map) = prev_opt
        .clone()
        .unwrap_or_else(|| (now, HashMap::new()));
    let dt_ms = now.saturating_duration_since(prev_time).as_millis();

    // 2. 枚举所有进程 + 采集当前快照
    let (processes_base, snap_map) = enumerate_with_snapshots()?;

    // 3. 只要间隔 >= 阈值, 就计算一次速率 (首次调用时 prev_map 为空, 所有速率仍为 0 —— 这是预期, 用作基线锚点)
    let mut rate_guard = RATE_CACHE.lock().map_err(|e| e.to_string())?;
    if dt_ms >= MIN_SAMPLE_INTERVAL_MS {
        let dt_sec = (dt_ms as f64) / 1000.0;
        for (pid, snap) in snap_map.iter() {
            let prev = prev_map.get(pid).copied();

            // Disk delta
            let (disk_rb, disk_wb) = match prev {
                Some(p) => (
                    snap.disk_read_bytes.saturating_sub(p.disk_read_bytes),
                    snap.disk_write_bytes.saturating_sub(p.disk_write_bytes),
                ),
                None => (0, 0),
            };

            // Net delta (BytesIn / BytesOut, Win11 24H2+; 老版本 snap 字段恒 0)
            let (net_in_d, net_out_d) = match prev {
                Some(p) => (
                    snap.net_in_bytes.saturating_sub(p.net_in_bytes),
                    snap.net_out_bytes.saturating_sub(p.net_out_bytes),
                ),
                None => (0, 0),
            };

            // CPU %
            let cpu_percent = match prev {
                Some(p) => {
                    let delta = snap.cpu_total_ticks.saturating_sub(p.cpu_total_ticks) as f64;
                    let delta_cpu_sec = delta * 1e-7;
                    ((delta_cpu_sec / dt_sec) * 100.0) as f32
                }
                None => 0.0,
            };
            let cpu_percent = cpu_percent.max(0.0).min(nproc * 100.0 * 1.1);

            rate_guard.insert(
                *pid,
                RateSample {
                    cpu_percent,
                    disk_read_bps: (disk_rb as f64 / dt_sec) as u64,
                    disk_write_bps: (disk_wb as f64 / dt_sec) as u64,
                    net_in_bps: (net_in_d as f64 / dt_sec) as u64,
                    net_out_bps: (net_out_d as f64 / dt_sec) as u64,
                },
            );
        }
        rate_guard.retain(|pid, _| snap_map.contains_key(pid));
    } else if prev_opt.is_none() {
        // 首次调用: 清理已失效 PID (只保留当前存在的进程)
        rate_guard.retain(|pid, _| snap_map.contains_key(pid));
    }

    // 4. ★关键★: 无论是否触发了速率计算, 都要把本次快照写入缓存, 保证下一次有 baseline 可做差分
    //   (之前的 bug 是只在 if 内部写缓存, 导致首次调用永远不写入, 后续的所有 dt_ms 永远为 0)
    {
        let mut cache = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((now, snap_map));
    }

    // 组装最终结果
    let mut out: Vec<ProcessInfo> = Vec::with_capacity(processes_base.len());
    for pb in processes_base {
        let rs = rate_guard.get(&pb.pid).copied().unwrap_or_default();
        out.push(ProcessInfo {
            pid: pb.pid,
            name: pb.name,
            affinity_mask: pb.affinity_mask,
            system_affinity_mask: pb.system_affinity_mask,
            parent_pid: pb.parent_pid,
            access_denied: pb.access_denied,
            cpu_usage_percent: rs.cpu_percent,
            memory_bytes: pb.memory_bytes,
            disk_read_bps: rs.disk_read_bps,
            disk_write_bps: rs.disk_write_bps,
            net_in_bps: rs.net_in_bps,
            net_out_bps: rs.net_out_bps,
        });
    }

    out.sort_by(|a, b| {
        let a_sys = is_system_process(a.pid);
        let b_sys = is_system_process(b.pid);
        match (a_sys, b_sys) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.pid.cmp(&b.pid),
        }
    });
    Ok(out)
}

// ============================================================
// 进程列表缓存 (加速首屏: 下次启动直接显示上次的进程列表, 慢字段全 0)
// ============================================================

fn processes_cache_path(base_dir: &std::path::Path) -> std::path::PathBuf {
    base_dir.join("processes_cache.json")
}

/// 读取上次保存的进程列表缓存。只含 PID/name/parent_pid, 慢字段全 0。
/// 返回 Ok(None) 表示无缓存或解析失败, 调用方回退到 list_processes_light。
pub fn load_processes_cache(base_dir: &std::path::Path) -> Option<Vec<ProcessInfo>> {
    let path = processes_cache_path(base_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<Vec<ProcessInfo>>(&raw).ok()
}

/// 把进程列表写入缓存 (只存基础字段, 慢字段也一起存, 下次读出来直接显示)。
/// 失败静默忽略 (缓存只是优化, 不影响功能)。
pub fn save_processes_cache(base_dir: &std::path::Path, processes: &[ProcessInfo]) {
    let _ = std::fs::create_dir_all(base_dir);
    let path = processes_cache_path(base_dir);
    if let Ok(json) = serde_json::to_string(processes) {
        let _ = std::fs::write(&path, json);
    }
}

struct ProcessBase {
    pid: u32,
    name: String,
    affinity_mask: Option<String>,
    system_affinity_mask: Option<String>,
    parent_pid: u32,
    access_denied: bool,
    memory_bytes: u64,
}

/// 轻量快扫返回的条目: 只含 PID / name / parent_pid, 不做 OpenProcess (<20ms)
#[derive(Clone, Debug)]
struct RawEntry {
    pid: u32,
    name: String,
    parent_pid: u32,
}

/// 纯 Toolhelp SNAPPROCESS 快扫 —— 只取 PID / name / parent_pid。
/// 不做任何 OpenProcess / NtQuery → ~10ms 量级。
fn list_basic_entries() -> Result<Vec<RawEntry>, String> {
    let pe32_size = size_of::<PROCESSENTRY32W>() as u32;
    let mut raw: Vec<RawEntry> = Vec::with_capacity(512);

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| format!("CreateToolhelp32Snapshot 失败: {}", e))?;

        let mut entry = PROCESSENTRY32W {
            dwSize: pe32_size,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                entry.dwSize = pe32_size;
                raw.push(RawEntry {
                    pid: entry.th32ProcessID,
                    name: pcwstr_to_string(&entry.szExeFile),
                    parent_pid: entry.th32ParentProcessID,
                });

                entry.dwSize = pe32_size;
                match Process32NextW(snapshot, &mut entry) {
                    Ok(()) => continue,
                    Err(e) => {
                        let last_err = WIN32_ERROR::from_error(&e).map(|x| x.0).unwrap_or(0);
                        if last_err == 18 { break; } // ERROR_NO_MORE_FILES
                        let mut next_ok = false;
                        for _ in 0..5 {
                            entry.dwSize = pe32_size;
                            match Process32NextW(snapshot, &mut entry) {
                                Ok(()) => { next_ok = true; break; }
                                Err(e2) => {
                                    let err2 = WIN32_ERROR::from_error(&e2).map(|x| x.0).unwrap_or(0);
                                    if err2 == 18 { break; }
                                }
                            }
                        }
                        if next_ok { continue; }
                        break;
                    }
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }

    if !raw.iter().any(|r| r.pid == 0) {
        raw.insert(
            0,
            RawEntry {
                pid: 0,
                name: "System Idle Process".to_string(),
                parent_pid: 0,
            },
        );
    }

    Ok(raw)
}

fn enumerate_with_snapshots() -> Result<(Vec<ProcessBase>, HashMap<u32, ProcessSnapshot>), String> {
    let raw = list_basic_entries()?;

    // =====================================================================
    // Phase 2: 并行遍历 raw 列表, 打开每个进程读亲和性 + 指标
    //          (和 Toolhelp snapshot 完全解耦)
    //          ★性能关键★: 343 个进程 × 4 syscall = ~1.3k 次 OpenProcess
    //          单线程需要 ~1-2s, 并行后利用 syscall 并发可降到 ~300ms
    //          用 std::thread::scope + 按 chunk 分批, 不引入 rayon 依赖
    // =====================================================================
    let nproc = get_number_of_processors() as usize;
    let n_threads = nproc.clamp(2, 8);
    let chunk_size = (raw.len() + n_threads - 1) / n_threads;
    let chunks: Vec<&[RawEntry]> = raw.chunks(chunk_size).collect();

    let mut all_results: Vec<(usize, ProcessBase, ProcessSnapshot)> =
        Vec::with_capacity(raw.len());

    std::thread::scope(|s| {
        let handles: Vec<std::thread::ScopedJoinHandle<'_, Vec<(usize, ProcessBase, ProcessSnapshot)>>> =
            chunks.iter().enumerate().map(|(chunk_idx, chunk)| {
                let chunk_start = chunk_idx * chunk_size;
                s.spawn(move || {
                    let mut out: Vec<(usize, ProcessBase, ProcessSnapshot)> =
                        Vec::with_capacity(chunk.len());
                    for (i, r) in chunk.iter().enumerate() {
                        let global_idx = chunk_start + i;
                        let (affinity_mask, system_affinity_mask, access_denied, mem_bytes, snap) =
                            match open_handle_for_stats(r.pid) {
                                (None, _) => (None, None, true, 0, ProcessSnapshot::default()),
                                (Some(h), partially_denied) => {
                                    let (pm, sm) = read_affinity_with_handle(h);
                                    let m = read_metrics_for_handle(h);
                                    close_handle(h);
                                    let snap = ProcessSnapshot {
                                        cpu_total_ticks: m.cpu_total_ticks.unwrap_or(0),
                                        disk_read_bytes: m.disk_read_bytes.unwrap_or(0),
                                        disk_write_bytes: m.disk_write_bytes.unwrap_or(0),
                                        io_read_bytes: m.io_read_bytes.unwrap_or(0),
                                        io_write_bytes: m.io_write_bytes.unwrap_or(0),
                                        net_in_bytes: m.net_in_bytes.unwrap_or(0),
                                        net_out_bytes: m.net_out_bytes.unwrap_or(0),
                                    };
                                    let denied =
                                        partially_denied || (pm.is_none() && m.cpu_total_ticks.is_none());
                                    (pm.map(mask_to_hex), sm.map(mask_to_hex), denied, m.working_set_bytes, snap)
                                }
                            };

                        out.push((
                            global_idx,
                            ProcessBase {
                                pid: r.pid,
                                name: r.name.clone(),
                                affinity_mask,
                                system_affinity_mask,
                                parent_pid: r.parent_pid,
                                access_denied,
                                memory_bytes: mem_bytes,
                            },
                            snap,
                        ));
                    }
                    out
                })
            }).collect();

        for h in handles {
            if let Ok(v) = h.join() {
                all_results.extend(v);
            }
        }
    });

    // 按原始顺序还原 (并行是按 chunk 处理的, 顺序可能乱)
    all_results.sort_by_key(|(i, _, _)| *i);

    let mut bases: Vec<ProcessBase> = Vec::with_capacity(all_results.len());
    let mut snaps: HashMap<u32, ProcessSnapshot> = HashMap::with_capacity(all_results.len());
    for (_, base, snap) in all_results {
        snaps.insert(base.pid, snap);
        bases.push(base);
    }

    Ok((bases, snaps))
}

// ---------- 亲和性读写 ----------

#[allow(dead_code)]
pub fn get_process_affinity(pid: u32) -> Result<(Option<u64>, Option<u64>), String> {
    unsafe {
        let handle = open_for_query(pid)?;
        let (pm, sm) = read_affinity_with_handle(handle);
        let _ = CloseHandle(handle);
        Ok((pm, sm))
    }
}

pub fn set_process_affinity(pid: u32, mask: u64) -> Result<(), String> {
    unsafe {
        let handle = OpenProcess(PROCESS_SET_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess(PROCESS_SET_INFORMATION) 失败 (PID {}): {}", pid, e))?;

        let result = SetProcessAffinityMask(handle, mask as usize);
        let _ = CloseHandle(handle);
        if let Err(e) = result {
            return Err(format!(
                "SetProcessAffinityMask 失败 (PID {}, mask=0x{:X}): {}",
                pid, mask, e
            ));
        }
        Ok(())
    }
}

pub fn parse_hex_mask(s: &str) -> Result<u64, String> {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if trimmed.is_empty() {
        return Err(format!("mask 不能为空 (原始输入: \"{}\")", s));
    }
    if trimmed.len() > 16 {
        return Err(format!(
            "mask 超过 64 位 ({} 位十六进制数字): \"{}\"",
            trimmed.len(),
            s
        ));
    }
    u64::from_str_radix(trimmed, 16)
        .map_err(|e| format!("无法解析 mask \"{}\": {}", s, e))
}

// ---------- 内部辅助 ----------

#[allow(dead_code)]
fn open_for_query(pid: u32) -> Result<HANDLE, String> {
    unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 失败 (PID {}): {}", pid, e))
    }
}

fn read_affinity_with_handle(handle: HANDLE) -> (Option<u64>, Option<u64>) {
    unsafe {
        let mut process_mask: usize = 0;
        let mut system_mask: usize = 0;
        match GetProcessAffinityMask(handle, &mut process_mask, &mut system_mask) {
            Ok(_) => (Some(process_mask as u64), Some(system_mask as u64)),
            Err(_) => (None, None),
        }
    }
}

fn pcwstr_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn is_system_process(pid: u32) -> bool {
    pid == 0 || pid == 4
}

// =========================================================================
// 分步指标推送 (为前端提供 4 波异步事件, 代替每秒全量拉取 + 整表重渲)
//
// 前端通过 `start_metrics_stream` 命令启动后台线程, 之后每秒做一次采集,
// 按 CPU → 内存 → 磁盘 → 网络 的顺序分 4 波 Tauri event 推给前端, 视觉上
// 各列从左到右独立更新, 不触发整表「一闪一闪」。
// =========================================================================

/// 单波指标事件 payload (紧凑 tuple 编码, 尽量小)
/// wave 1: batch 每一项 = [pid, cpu_percent]
/// wave 2: batch 每一项 = [pid, memory_bytes]
/// wave 3: batch 每一项 = [pid, disk_read_bps, disk_write_bps]
/// wave 4: batch 每一项 = [pid, net_in_bps, net_out_bps]
#[derive(Serialize, Clone, Debug)]
pub struct MetricsWaveEvent {
    pub wave: u8,
    /// number[][] — 避免引入额外依赖类型, 直接用 serde_json 友好的嵌套数组
    pub batch: Vec<Vec<serde_json::Value>>,
}

/// 结构变化事件 (PID 集合启动/退出, 或 name/affinity/ppid 这些慢变字段更新)
#[derive(Serialize, Clone, Debug)]
pub struct ProcessDiffEvent {
    /// 结构字段快照 (不含速率字段), 新增或可能更新的进程
    pub upserts: Vec<ProcessBaseSnapshot>,
    /// 本轮已退出、前端应 splice 掉的 pid
    pub removed_pids: Vec<u32>,
}

/// 不含速率字段的 ProcessBase 快照 (给前端 applyProcessDiff 用)
#[derive(Serialize, Clone, Debug)]
pub struct ProcessBaseSnapshot {
    pub pid: u32,
    pub name: String,
    pub affinity_mask: Option<String>,
    pub system_affinity_mask: Option<String>,
    pub parent_pid: u32,
    pub access_denied: bool,
    pub memory_bytes: u64,
}

/// 一次完整采样的产出: 速率 + 结构信息 + 差异。调用方按 4 次 emit 分发。
pub struct MetricsTick {
    pub rates: HashMap<u32, RateSample>,
    pub memory_by_pid: HashMap<u32, u64>,
    /// 本轮活着的 PID 全集 (用于下一轮 removed 判定)
    pub alive_pids: Vec<u32>,
    pub removed_pids: Vec<u32>,
    /// 新出现 / 结构字段可能变化的进程 (带结构快照)
    pub upserts: Vec<ProcessBaseSnapshot>,
}

/// 把 `list_processes` 里「采集 + 差分计算 + 写缓存基线」的核心逻辑抽出来复用,
/// 返回结构化的 MetricsTick, 不做 Vec<ProcessInfo> 组装 (那一步体积大、只在首屏全量加载需要)。
pub fn collect_metrics_tick() -> Result<MetricsTick, String> {
    let now = Instant::now();
    let nproc = get_number_of_processors() as f32;

    let prev_opt: Option<(Instant, HashMap<u32, ProcessSnapshot>)> = {
        let guard = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        guard.clone()
    };
    let (prev_time, prev_map) = prev_opt
        .clone()
        .unwrap_or_else(|| (now, HashMap::new()));
    let dt_ms = now.saturating_duration_since(prev_time).as_millis();

    let (processes_base, snap_map) = enumerate_with_snapshots()?;

    // 本轮 alive pid 集合 (用于 removed 判定)
    let alive_set: std::collections::HashSet<u32> = snap_map.keys().copied().collect();
    let alive_pids: Vec<u32> = alive_set.iter().copied().collect();

    // ---- removed ----
    let removed_pids: Vec<u32> = prev_opt
        .as_ref()
        .map(|(_, pm)| pm.keys().copied().filter(|p| !alive_set.contains(p)).collect())
        .unwrap_or_default();

    // ---- rates ----
    let mut rate_guard = RATE_CACHE.lock().map_err(|e| e.to_string())?;
    if dt_ms >= MIN_SAMPLE_INTERVAL_MS {
        let dt_sec = (dt_ms as f64) / 1000.0;
        for (pid, snap) in snap_map.iter() {
            let prev = prev_map.get(pid).copied();
            let (disk_rb, disk_wb) = match prev {
                Some(p) => (
                    snap.disk_read_bytes.saturating_sub(p.disk_read_bytes),
                    snap.disk_write_bytes.saturating_sub(p.disk_write_bytes),
                ),
                None => (0, 0),
            };
            let (net_in_d, net_out_d) = match prev {
                Some(p) => (
                    snap.net_in_bytes.saturating_sub(p.net_in_bytes),
                    snap.net_out_bytes.saturating_sub(p.net_out_bytes),
                ),
                None => (0, 0),
            };
            let cpu_percent = match prev {
                Some(p) => {
                    let delta = snap.cpu_total_ticks.saturating_sub(p.cpu_total_ticks) as f64;
                    let delta_cpu_sec = delta * 1e-7;
                    ((delta_cpu_sec / dt_sec) * 100.0) as f32
                }
                None => 0.0,
            };
            let cpu_percent = cpu_percent.max(0.0).min(nproc * 100.0 * 1.1);
            rate_guard.insert(
                *pid,
                RateSample {
                    cpu_percent,
                    disk_read_bps: (disk_rb as f64 / dt_sec) as u64,
                    disk_write_bps: (disk_wb as f64 / dt_sec) as u64,
                    net_in_bps: (net_in_d as f64 / dt_sec) as u64,
                    net_out_bps: (net_out_d as f64 / dt_sec) as u64,
                },
            );
        }
        rate_guard.retain(|pid, _| alive_set.contains(pid));
    } else if prev_opt.is_none() {
        rate_guard.retain(|pid, _| alive_set.contains(pid));
    }
    // rates 拷贝一份后立刻释放锁
    let rates: HashMap<u32, RateSample> = rate_guard.clone();
    drop(rate_guard);

    // 写 baseline 缓存 (关键, 和 list_processes 保持一致)
    {
        let mut cache = SNAPSHOT_CACHE.lock().map_err(|e| e.to_string())?;
        *cache = Some((now, snap_map));
    }

    // ---- memory & upserts ----
    // 为了避免每轮都扫 343 条进程把整个结构 JSON 推一遍, upserts 只推:
    //  a) 新 PID (prev_map 中没有)
    //  b) 内存 Working Set 有显著变化的 (用户也希望内存逐步更新, 这里阈值 4KB 防止噪声)
    //  c) 任何一轮都会把 name/ppid/affinity 最新值一并附上
    static PREV_STRUCT_MEMORY: Lazy<Mutex<HashMap<u32, u64>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    let mut prev_mem = PREV_STRUCT_MEMORY.lock().map_err(|e| e.to_string())?;

    let mut memory_by_pid = HashMap::with_capacity(processes_base.len());
    let mut upserts = Vec::new();
    for pb in processes_base.iter() {
        memory_by_pid.insert(pb.pid, pb.memory_bytes);
        let is_new = !prev_map.contains_key(&pb.pid);
        let prev_mem_bytes = *prev_mem.get(&pb.pid).unwrap_or(&0);
        let mem_changed = pb.memory_bytes.abs_diff(prev_mem_bytes) > 4 * 1024;
        if is_new || mem_changed {
            upserts.push(ProcessBaseSnapshot {
                pid: pb.pid,
                name: pb.name.clone(),
                affinity_mask: pb.affinity_mask.clone(),
                system_affinity_mask: pb.system_affinity_mask.clone(),
                parent_pid: pb.parent_pid,
                access_denied: pb.access_denied,
                memory_bytes: pb.memory_bytes,
            });
            prev_mem.insert(pb.pid, pb.memory_bytes);
        }
    }
    // removed pids 同步清掉 prev_struct_memory
    for pid in removed_pids.iter() {
        prev_mem.remove(pid);
    }
    drop(prev_mem);

    Ok(MetricsTick {
        rates,
        memory_by_pid,
        alive_pids,
        removed_pids,
        upserts,
    })
}

/// 把一次 MetricsTick 按 wave 编码成 4 个推送事件 + 1 个 diff 事件。
/// 这样前端可以按「CPU → 内存 → 磁盘 → 网络」的节奏分步更新, 避免整表闪烁。
pub fn build_wave_events(tick: &MetricsTick) -> (Vec<MetricsWaveEvent>, ProcessDiffEvent) {
    // wave 1: pid + cpu_percent
    let wave1: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(
                    (rs.cpu_percent * 100.0).round() as i64 as f64 / 100.0,
                ),
            ]
        })
        .collect();

    // wave 2: pid + memory_bytes
    let wave2: Vec<Vec<serde_json::Value>> = tick
        .memory_by_pid
        .iter()
        .map(|(pid, mem)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(*mem),
            ]
        })
        .collect();

    // wave 3: pid + disk_read_bps + disk_write_bps
    let wave3: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(rs.disk_read_bps),
                serde_json::Value::from(rs.disk_write_bps),
            ]
        })
        .collect();

    // wave 4: pid + net_in_bps + net_out_bps
    let wave4: Vec<Vec<serde_json::Value>> = tick
        .rates
        .iter()
        .map(|(pid, rs)| {
            vec![
                serde_json::Value::from(*pid),
                serde_json::Value::from(rs.net_in_bps),
                serde_json::Value::from(rs.net_out_bps),
            ]
        })
        .collect();

    let waves = vec![
        MetricsWaveEvent { wave: 1, batch: wave1 },
        MetricsWaveEvent { wave: 2, batch: wave2 },
        MetricsWaveEvent { wave: 3, batch: wave3 },
        MetricsWaveEvent { wave: 4, batch: wave4 },
    ];

    let diff = ProcessDiffEvent {
        upserts: tick.upserts.clone(),
        removed_pids: tick.removed_pids.clone(),
    };

    (waves, diff)
}

// ---------- 指标推送流的后台线程控制 ----------

static STREAM_RUNNING: AtomicBool = AtomicBool::new(false);
static STREAM_JOIN_HANDLE: Lazy<Mutex<Option<JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));

pub fn start_metrics_stream_in_thread<EmitFn>(
    interval_ms: u32,
    emit: EmitFn,
) -> Result<(), String>
where
    EmitFn: Fn(&str, serde_json::Value) -> Result<(), String> + Send + 'static,
{
    if STREAM_RUNNING.load(Relaxed) {
        return Ok(());
    }
    STREAM_RUNNING.store(true, Relaxed);

    let interval = std::time::Duration::from_millis(interval_ms.max(100) as u64);

    let handle = std::thread::Builder::new()
        .name("cpum-metrics-stream".into())
        .spawn(move || {
            // ★第一轮延迟 1 秒★: 避免和首屏 list_processes 并发扫描 343 个进程,
            // 导致系统被 OpenProcess syscall 淹没卡死
            let wave_gap = std::time::Duration::from_millis(5);
            let first_sleep = std::time::Duration::from_secs(1);
            let mut slept = std::time::Duration::ZERO;
            while slept < first_sleep && STREAM_RUNNING.load(Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                slept += std::time::Duration::from_millis(50);
            }
            loop {
                if !STREAM_RUNNING.load(Relaxed) {
                    break;
                }
                let round_start = Instant::now();

                match collect_metrics_tick() {
                    Ok(tick) => {
                        let (waves, diff) = build_wave_events(&tick);

                        for (i, w) in waves.iter().enumerate() {
                            if !STREAM_RUNNING.load(Relaxed) {
                                break;
                            }
                            let payload = match serde_json::to_value(w) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let _ = emit("process://metrics", payload);
                            // wave 1→2, 2→3, 3→4 之间插入 5ms 间隔, 给前端 1 帧的时间
                            if i < waves.len() - 1 {
                                std::thread::sleep(wave_gap);
                            }
                        }

                        // diff 放最后: 新增/退出的进程要等指标都先有一轮再同步
                        if STREAM_RUNNING.load(Relaxed) {
                            if let Ok(payload) = serde_json::to_value(&diff) {
                                let _ = emit("process://processes-diff", payload);
                            }
                        }
                    }
                    Err(_) => {
                        // 采集失败就跳过这一轮, 不 crash 线程
                    }
                }

                if !STREAM_RUNNING.load(Relaxed) {
                    break;
                }
                // 保证间隔 ≈ interval_ms (若采集自身耗时就睡少一点)
                let elapsed = round_start.elapsed();
                if let Some(remaining) = interval.checked_sub(elapsed) {
                    // 分成小 sleep, 中途 stop 可以更快响应
                    let step = std::time::Duration::from_millis(50);
                    let mut slept = std::time::Duration::ZERO;
                    while slept < remaining && STREAM_RUNNING.load(Relaxed) {
                        std::thread::sleep(step.min(remaining - slept));
                        slept = slept.saturating_add(step);
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let mut guard = STREAM_JOIN_HANDLE.lock().map_err(|e| e.to_string())?;
    if let Some(old) = guard.replace(handle) {
        drop(old);
    }
    Ok(())
}

pub fn stop_metrics_stream_in_thread() -> Result<(), String> {
    STREAM_RUNNING.store(false, Relaxed);
    if let Some(handle) = STREAM_JOIN_HANDLE
        .lock()
        .map_err(|e| e.to_string())?
        .take()
    {
        // 最多等 400 ms (线程可能卡在 50 ms 细粒度 sleep 里)
        let _ = std::thread::spawn(move || {
            let _ = handle.join();
        });
    }
    Ok(())
}

/// 亲和性写入成功时, 后端直接推一条事件给前端, 前端就不必再整表全量刷新。
pub fn build_affinity_updated_event(pid: u32, mask: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "pid": pid,
        "affinity_mask": mask,
    })
}

// ============================================================
// 网络 IO 诊断 (用于验证 NtQueryInformationProcess(ProcessNetworkIoCounters=114)
// 在当前 Windows 版本上是否真的返回数据 — Win11 24H2+ 才支持)
// ============================================================

/// 把 NTSTATUS 数值映射成可读名称 (覆盖最常见的几种)
fn nt_status_name(status: u32) -> &'static str {
    match status {
        0 => "STATUS_SUCCESS",
        0xC0000003 => "STATUS_INVALID_INFO_CLASS",
        0xC0000004 => "STATUS_INFO_LENGTH_MISMATCH",
        0xC0000005 => "STATUS_ACCESS_VIOLATION",
        0xC000000D => "STATUS_INVALID_PARAMETER",
        0xC0000022 => "STATUS_ACCESS_DENIED",
        0xC0000225 => "STATUS_NOT_FOUND",
        0xC0000008 => "STATUS_INVALID_HANDLE",
        _ => "UNKNOWN",
    }
}

/// 探测当前所有进程的网络 IO 计数, 输出可读的诊断文本。
///
/// 用于定位「网络列显示 0」的原因:
///  - 如果 NtQueryInformationProcess 解析失败 → ntdll 加载问题
///  - 如果所有调用都返回 STATUS_INVALID_INFO_CLASS → 当前 Windows 版本不支持 (需 Win11 24H2+)
///  - 如果 STATUS_SUCCESS 但所有 BytesIn/BytesOut 都是 0 → 当前确实没有网络活动
///  - 如果有非零值 → 后端正常工作, 问题在前端
pub fn dump_net_io_probe() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "=== Network IO Probe (NtQueryInformationProcess ProcessNetworkIoCounters=114) ==="
    );
    let _ = writeln!(out, "Info class: {} (ProcessNetworkIoCounters)", PROCESS_NETWORK_IO_COUNTERS_CLASS);
    let _ = writeln!(
        out,
        "Struct size: {} bytes (ProcessNetworkCounters {{ u64 BytesIn, u64 BytesOut }})",
        size_of::<ProcessNetworkCounters>()
    );

    let nt_fn_opt = *NT_QUERY_PROCESS;
    let nt_fn = match nt_fn_opt {
        None => {
            let _ = writeln!(
                out,
                "NtQueryInformationProcess: NOT RESOLVED — ntdll!NtQueryInformationProcess 找不到, 网络列恒 0"
            );
            return out;
        }
        Some(f) => {
            let _ = writeln!(out, "NtQueryInformationProcess: resolved OK");
            f
        }
    };

    let entries = match list_basic_entries() {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(out, "list_basic_entries FAILED: {}", e);
            return out;
        }
    };

    let mut total_probed: u32 = 0;
    let mut total_success: u32 = 0;
    let mut total_failed: u32 = 0;
    let mut total_access_denied: u32 = 0;
    let mut total_nonzero: u32 = 0;
    let mut nonzero_lines: Vec<String> = Vec::new();
    let mut failed_lines: Vec<String> = Vec::new();
    let mut first_status_codes: Vec<(u32, u32, String)> = Vec::new(); // (status_code, count, name)

    for entry in &entries {
        let pid = entry.pid;
        if pid == 0 {
            // System Idle Process 没有 handle, 跳过
            continue;
        }
        total_probed += 1;

        let handle = match open_handle(pid, PROCESS_QUERY_INFORMATION) {
            Some(h) => h,
            None => {
                total_access_denied += 1;
                total_failed += 1;
                if failed_lines.len() < 20 {
                    failed_lines.push(format!(
                        "PID {:>6} ({:<30}) OpenProcess FAILED (access denied)",
                        pid, entry.name
                    ));
                }
                continue;
            }
        };

        let (status_u32, bytes_returned, pnc) = unsafe {
            let mut pnc: ProcessNetworkCounters = std::mem::zeroed();
            let mut bytes_returned: u32 = 0;
            let status: i32 = nt_fn(
                handle,
                PROCESS_NETWORK_IO_COUNTERS_CLASS,
                &mut pnc as *mut _ as *mut std::ffi::c_void,
                size_of::<ProcessNetworkCounters>() as u32,
                &mut bytes_returned,
            );
            (status as u32, bytes_returned, pnc)
        };
        close_handle(handle);

        // 收集第一个出现的每个 status code (便于了解失败原因分布)
        if !first_status_codes.iter().any(|(s, _, _)| *s == status_u32) {
            first_status_codes.push((status_u32, 0, nt_status_name(status_u32).to_string()));
        }
        let slot = first_status_codes.iter_mut().find(|(s, _, _)| *s == status_u32);
        if let Some((_, count, _)) = slot {
            *count += 1;
        }

        if status_u32 == 0 {
            total_success += 1;
            if pnc.bytes_in > 0 || pnc.bytes_out > 0 {
                total_nonzero += 1;
                if nonzero_lines.len() < 50 {
                    nonzero_lines.push(format!(
                        "PID {:>6} ({:<30}) bytes_returned={:>3} BytesIn={:>14} BytesOut={:>14}",
                        pid, entry.name, bytes_returned, pnc.bytes_in, pnc.bytes_out
                    ));
                }
            }
        } else {
            total_failed += 1;
            if failed_lines.len() < 20 {
                failed_lines.push(format!(
                    "PID {:>6} ({:<30}) status={:#010X} ({}) bytes_returned={}",
                    pid, entry.name, status_u32, nt_status_name(status_u32), bytes_returned
                ));
            }
        }
    }

    let _ = writeln!(out, "");
    let _ = writeln!(out, "=== Summary ===");
    let _ = writeln!(out, "Probed:              {} processes (excl. PID 0 idle)", total_probed);
    let _ = writeln!(out, "STATUS_SUCCESS:      {}", total_success);
    let _ = writeln!(out, "Failed:              {} (其中 OpenProcess access denied = {})", total_failed, total_access_denied);
    let _ = writeln!(out, "Non-zero BytesIn/Out: {}", total_nonzero);

    if !first_status_codes.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== NTSTATUS 分布 ===");
        // 按数量降序排
        first_status_codes.sort_by(|a, b| b.1.cmp(&a.1));
        for (status, count, name) in &first_status_codes {
            let _ = writeln!(out, "  {:#010X} ({})  × {}", status, name, count);
        }
    }

    if !nonzero_lines.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== 有非零网络字节的进程 (top 50) ===");
        for line in &nonzero_lines {
            let _ = writeln!(out, "{}", line);
        }
    } else {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== 没有任何进程报告非零网络字节 ===");
        let _ = writeln!(out, "可能原因:");
        let _ = writeln!(out, "  1. 当前 Windows 版本不支持 ProcessNetworkIoCounters (需要 Win11 24H2+ / Build 26100+)");
        let _ = writeln!(out, "  2. 所有进程的 NTSTATUS 都是失败 (见上面 NTSTATUS 分布和 Failed probes)");
        let _ = writeln!(out, "  3. 系统启动后还没产生任何网络流量 (不太可能, System 进程通常有非零值)");
    }

    if !failed_lines.is_empty() {
        let _ = writeln!(out, "");
        let _ = writeln!(out, "=== 失败样本 (前 20 条) ===");
        for line in &failed_lines {
            let _ = writeln!(out, "{}", line);
        }
    }

    out
}


// ============================================================
//   Unit Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_mask_basic() {
        assert_eq!(parse_hex_mask("0xFF").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("FF").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("0xff").unwrap(), 0xFF);
        assert_eq!(parse_hex_mask("0xFFFFFFFF").unwrap(), 0xFFFFFFFF);
    }

    #[test]
    fn parse_hex_mask_full_64bit() {
        assert_eq!(parse_hex_mask("0xFFFFFFFFFFFFFFFF").unwrap(), u64::MAX);
    }

    #[test]
    fn parse_hex_mask_empty_string() {
        assert!(parse_hex_mask("").is_err());
        assert!(parse_hex_mask("  ").is_err());
        assert!(parse_hex_mask("0x").is_err());
        assert!(parse_hex_mask("0X").is_err());
    }

    #[test]
    fn parse_hex_mask_too_long() {
        assert!(parse_hex_mask("0xFFFFFFFFFFFFFFFF0").is_err());
        assert!(parse_hex_mask("000000000000000000").is_err());
    }

    #[test]
    fn parse_hex_mask_invalid_chars() {
        assert!(parse_hex_mask("0xGHIJ").is_err());
        assert!(parse_hex_mask("not_a_hex").is_err());
    }

    #[test]
    fn parse_hex_mask_whitespace() {
        assert_eq!(parse_hex_mask("  0xFF  ").unwrap(), 0xFF);
    }

    #[test]
    fn mask_to_hex_roundtrip() {
        for v in [0u64, 1, 0xFF, 0xFFFF, 0xFFFFFFFF, u64::MAX] {
            let hex = mask_to_hex(v);
            let parsed = parse_hex_mask(&hex).unwrap();
            assert_eq!(parsed, v, "roundtrip failed for {:#X}", v);
        }
    }
}
