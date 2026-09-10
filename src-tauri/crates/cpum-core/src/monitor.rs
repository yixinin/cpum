//! Lightweight monitor: process CPU differential sampling + foreground
//! process identification.
//!
//! The "eyes" of the dynamic optimization engine (ProBalance) - used
//! service-side only, no dependency on the GUI's metrics stream:
//!  - [`CpuSampler`]: ToolHelp enumeration + GetProcessTimes differential
//!    -> per-process CPU% (single-core baseline)
//!  - [`get_foreground_pid`]: foreground window -> PID (cross-session
//!    desktop switch trick, see the function's comments)

use std::collections::HashMap;
use std::mem::size_of;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId,
};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};

// =========================================================================
// Process CPU differential sampling
// =========================================================================

/// One process's sample result.
pub struct ProcSample {
    pub pid: u32,
    /// Executable file name (e.g. "codex.exe").
    pub name: String,
    /// CPU usage, single-core baseline: 100.0 = one full logical core,
    /// 8 cores fully used = 800.0.
    pub cpu_percent: f32,
    /// Full executable path (only when the sample request asked to resolve
    /// it and the resolution succeeded; needed by Path-type protection
    /// rules).
    pub image_path: Option<String>,
}

/// Differential CPU sampler for processes.
///
/// Each [`tick`](CpuSampler::tick) enumerates all processes and reads the
/// cumulative CPU time (kernel + user), then diffs against the previous
/// snapshot. The first round only establishes the baseline (all CPUs read
/// 0); rates become available from the second round on. For ~300 processes,
/// the per-round OpenProcess + GetProcessTimes cost is a few milliseconds -
/// well within a 1-second service period.
pub struct CpuSampler {
    last: HashMap<u32, u64>,
    last_tick: Option<Instant>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    pub fn new() -> Self {
        Self { last: HashMap::new(), last_tick: None }
    }

    /// Take one sample; enumeration failure returns Err (caller may ignore
    /// and retry next round). Processes whose handle cannot be opened
    /// (protected / already exited) are absent from the result - the
    /// engine uses this fact to determine liveness.
    ///
    /// When `include_paths=true`, the full path is resolved on the same
    /// handle (needed by Path-type protection rules; each OpenProcess adds
    /// one QueryFullProcessImageNameW call with no extra handle cost).
    /// The common case with no Path rules passes false for zero overhead.
    pub fn tick(&mut self, include_paths: bool) -> Result<Vec<ProcSample>, String> {
        let now = Instant::now();
        let wall_secs = self
            .last_tick
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);

        let mut current: HashMap<u32, u64> = HashMap::new();
        let mut samples: Vec<ProcSample> = Vec::new();

        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(|e| format!("CreateToolhelp32Snapshot failed: {e}"))?;
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let pid = entry.th32ProcessID;
                    if let Some((total, image_path)) = process_sample(pid, include_paths) {
                        let prev = self.last.get(&pid).copied();
                        current.insert(pid, total);
                        // Single-core baseline CPU%: process CPU time delta
                        // / wall-clock delta x 100.
                        let cpu_percent = match (prev, wall_secs > 0.0) {
                            (Some(p), true) => {
                                (total.saturating_sub(p) as f64 / wall_secs * 100.0) as f32
                            }
                            _ => 0.0,
                        };
                        let name_len =
                            entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                        let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
                        samples.push(ProcSample { pid, name, cpu_percent, image_path });
                    }
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
        }

        self.last = current;
        self.last_tick = Some(now);
        Ok(samples)
    }
}

/// Open the process handle and read the cumulative CPU time (kernel + user,
/// 100-ns units). Optionally resolves the full path on the same handle.
/// Returns None when the handle cannot be opened or the process has exited.
fn process_sample(pid: u32, include_path: bool) -> Option<(u64, Option<String>)> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut _create = FILETIME::default();
        let mut _exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok =
            GetProcessTimes(handle, &mut _create, &mut _exit, &mut kernel, &mut user).is_ok();
        let image_path = if ok && include_path {
            query_image_path_with_handle(handle)
        } else {
            None
        };
        let _ = CloseHandle(handle);
        if !ok {
            return None;
        }
        Some((ft_to_u64(kernel) + ft_to_u64(user), image_path))
    }
}

/// Resolve the full executable path using an already-open handle (returns
/// None on failure).
fn query_image_path_with_handle(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::System::Threading::{
        QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    };
    use windows::core::PWSTR;
    unsafe {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        if result.is_ok() {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

fn ft_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64 & 0xFFFF_FFFF)
}

// =========================================================================
// Foreground process identification
// =========================================================================

/// Current foreground process PID (None if it cannot be determined).
///
/// The service runs in session 0, so calling `GetForegroundWindow` directly
/// only sees its own session's desktop (which has no interactive windows).
/// The standard trick: first switch the thread to the **input desktop**
/// (`OpenInputDesktop`; a LocalSystem service can open the active console
/// session's input desktop), then read the foreground window, then switch
/// back. On failure (lock screen / no interactive session / security
/// policy) it returns None - the engine treats this as "no foreground"
/// and does not trigger contention, gracefully degrading behavior.
pub struct ForegroundState {
    pub pid: Option<u32>,
    pub fullscreen: bool,
}

/// Read the foreground process and fullscreen presentation state from the
/// active input desktop. The latter is intentionally false on any API or
/// session-boundary failure: Game Mode must never activate on a guess.
pub fn get_foreground_state() -> ForegroundState {
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else {
            return ForegroundState { pid: None, fullscreen: false };
        };
        let old = GetThreadDesktop(GetCurrentThreadId()).ok();
        let switched = SetThreadDesktop(desktop).is_ok();
        let pid = if switched {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                None
            } else {
                let mut pid = 0u32;
                if GetWindowThreadProcessId(hwnd, Some(&mut pid)) != 0 && pid != 0 {
                    Some(pid)
                } else {
                    None
                }
            }
        } else {
            None
        };
        let fullscreen = switched
            && SHQueryUserNotificationState().map(|state| state == QUNS_RUNNING_D3D_FULL_SCREEN).unwrap_or(false);
        if switched {
            if let Some(old) = old {
                let _ = SetThreadDesktop(old);
            }
        }
        let _ = CloseDesktop(desktop);
        ForegroundState { pid, fullscreen }
    }
}

/// Current foreground process PID (None if it cannot be determined).
pub fn get_foreground_pid() -> Option<u32> {
    get_foreground_state().pid
}
