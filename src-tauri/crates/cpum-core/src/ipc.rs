//! Privileged bridge between the un-elevated GUI and the LocalSystem service.
//!
//! The desktop app runs as the invoking user (see the `asInvoker` manifest), so
//! it cannot touch processes that belong to another security context:
//! `OpenProcess` fails with `ERROR_ACCESS_DENIED` because a filtered UAC token
//! does not hold `SeDebugPrivilege`. The optional `CpumAffinityService` does
//! hold it, so the GUI can delegate those operations instead of asking the user
//! for a UAC prompt on every single change.
//!
//! Transport: a message-mode named pipe, one JSON request / one JSON response
//! per connection.
//!
//! # Authorization
//!
//! The pipe DACL is deliberately permissive (SYSTEM full control, read/write
//! for interactive users) because the service has no way to know which user
//! will connect. The real gate is a shared secret:
//!
//! * the GUI creates/reads `<rules dir>/bridge.token` (a random value) - that
//!   directory is the per-user `%APPDATA%\<bundle id>` folder, which only that
//!   user and SYSTEM can read;
//! * every request carries the token and the service compares it against the
//!   file it loaded at startup.
//!
//! A different local user can therefore open the pipe but cannot produce a
//! valid token, so they cannot ask a SYSTEM service to modify processes they do
//! not own. Note that this does **not** stop the machine administrator (they can
//! read anything anyway) and it cannot help with PPL / protected processes,
//! which even SYSTEM may not modify.
//!
//! # Shutdown
//!
//! [`serve`] blocks in `ConnectNamedPipe` and is meant to run on a background
//! thread of the service process; it is terminated when the process exits.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::engine::{self, ProcessApplyInfo};
use crate::procwin;
use crate::rule::RuleMode;

/// Name of the shared-secret file inside the per-user rules directory.
const TOKEN_FILE: &str = "bridge.token";
/// Requests and responses are small JSON documents; 64 KiB is plenty.
const MAX_MESSAGE: usize = 64 * 1024;

// ---------- Wire protocol ----------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check; also used to discover whether the bridge is up.
    Ping { token: String },
    /// Apply a set of per-group affinity masks (hex strings, e.g. `"0xFF"`).
    SetAffinity {
        token: String,
        pid: u32,
        masks: Vec<String>,
        mode: RuleMode,
    },
    /// Adjust any combination of the three priority classes.
    SetPriority {
        token: String,
        pid: u32,
        priority_class: Option<u32>,
        io_priority: Option<u32>,
        memory_priority: Option<u32>,
    },
    /// Re-apply every enabled rule using the service's own rules directory.
    ApplyRules { token: String },
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// Number of successful (process x rule) applications (`apply_rules`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub applied: Option<u32>,
    /// Per-process result of `apply_rules`, so the GUI can patch its rows.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub changed: Option<Vec<AppliedProcess>>,
    /// Priorities read back after `set_priority`. The GUI cannot always read
    /// them itself (reading a SYSTEM process needs the very privilege it just
    /// delegated), so the service returns them.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub priorities: Option<procwin::ProcessPriorities>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppliedProcess {
    pub pid: u32,
    pub mask_hex: Option<String>,
    pub priorities: Option<procwin::ProcessPriorities>,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, ..Default::default() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), ..Default::default() }
    }

    pub fn applied(applied: u32, changed: Vec<AppliedProcess>) -> Self {
        Self { ok: true, applied: Some(applied), changed: Some(changed), ..Default::default() }
    }
}

impl From<&ProcessApplyInfo> for AppliedProcess {
    fn from(value: &ProcessApplyInfo) -> Self {
        Self {
            pid: value.pid,
            mask_hex: value.mask_hex.clone(),
            priorities: value.priorities,
        }
    }
}

// ---------- Shared secret ----------

/// Return the bridge token for `dir`, creating it on first use.
///
/// Called by the GUI (which owns the directory). The service only reads it.
pub fn ensure_token(dir: &Path) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create rules directory: {e}"))?;
    let path = dir.join(TOKEN_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4());
    std::fs::write(&path, &token).map_err(|e| format!("failed to write bridge token: {e}"))?;
    Ok(token)
}

/// Read the token without creating it (service side).
pub fn read_token(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(TOKEN_FILE)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ---------- Handle wrapper ----------

struct Pipe(HANDLE);

// SAFETY: the wrapper owns the handle exclusively. It is created on the
// listener thread and then moved into exactly one connection thread, so no two
// threads ever use it concurrently.
unsafe impl Send for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

// ---------- Client (GUI side) ----------

/// Send one request to the service bridge and return its response.
///
/// Returns `Err` when the service is not installed / not running (the pipe does
/// not exist) so the caller can fall back to another strategy.
pub fn request(req: &Request) -> Result<Response, String> {
    let handle = unsafe {
        CreateFileW(
            w!(r"\\.\pipe\cpum-bridge-v1"),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| format!("service bridge unavailable: {e}"))?;
    let pipe = Pipe(handle);

    let payload = serde_json::to_vec(req).map_err(|e| format!("failed to encode request: {e}"))?;
    unsafe { WriteFile(pipe.0, Some(&payload), None, None) }
        .map_err(|e| format!("bridge write failed: {e}"))?;

    let mut buffer = vec![0u8; MAX_MESSAGE];
    let mut read = 0u32;
    unsafe { ReadFile(pipe.0, Some(&mut buffer[..]), Some(&mut read), None) }
        .map_err(|e| format!("bridge read failed: {e}"))?;

    serde_json::from_slice(&buffer[..read as usize])
        .map_err(|e| format!("bad bridge response: {e}"))
}

// ---------- Server (service side) ----------

/// Build a security descriptor for the pipe from [`PIPE_SDDL`].
///
/// The returned pointer stays allocated for the lifetime of the process (it is
/// intentionally leaked - one descriptor per process).
fn pipe_security_descriptor() -> Option<PSECURITY_DESCRIPTOR> {
    // GA = generic all for SYSTEM, GRGW = read/write for INTERACTIVE (locally
    // logged-on users). See the module docs: the token is the real gate.
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let result = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;;GA;;;SY)(A;;GRGW;;;IU)"),
            1,
            &mut sd,
            None,
        )
    };
    result.ok().map(|()| sd)
}

/// Listen on the pipe and dispatch requests until the process exits.
///
/// `rules_dir` is the directory the service was started with; it is used both
/// to locate the token file and to load rules for the `apply_rules` operation.
pub fn serve(rules_dir: PathBuf) {
    // The service process already enabled SeDebugPrivilege, but the helper is
    // cheap and keeps this function usable from any host.
    let _ = procwin::enable_debug_privilege();

    let Some(sd) = pipe_security_descriptor() else {
        eprintln!("cpum bridge: cannot build pipe security descriptor");
        return;
    };
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };

    loop {
        let handle = unsafe {
            CreateNamedPipeW(
                w!(r"\\.\pipe\cpum-bridge-v1"),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                MAX_MESSAGE as u32,
                MAX_MESSAGE as u32,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            eprintln!("cpum bridge: CreateNamedPipeW failed");
            return;
        }
        let pipe = Pipe(handle);

        // A client that connects and hangs up immediately can make this fail;
        // dropping the pipe instance and looping is the documented recovery.
        if unsafe { ConnectNamedPipe(pipe.0, None) }.is_err() {
            continue;
        }

        let dir = rules_dir.clone();
        std::thread::spawn(move || handle_connection(pipe, &dir));
    }
}

fn handle_connection(pipe: Pipe, rules_dir: &Path) {
    let mut buffer = vec![0u8; MAX_MESSAGE];
    let mut read = 0u32;

    let response = match unsafe { ReadFile(pipe.0, Some(&mut buffer[..]), Some(&mut read), None) } {
        Ok(()) => match serde_json::from_slice::<Request>(&buffer[..read as usize]) {
            Ok(request) => dispatch(request, rules_dir),
            Err(e) => Response::error(format!("bad request: {e}")),
        },
        Err(e) => Response::error(format!("read failed: {e}")),
    };

    let payload = serde_json::to_vec(&response).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    let _ = unsafe { WriteFile(pipe.0, Some(&payload), None, None) };
    let _ = unsafe { DisconnectNamedPipe(pipe.0) };
}

fn dispatch(request: Request, rules_dir: &Path) -> Response {
    let token = match &request {
        Request::Ping { token }
        | Request::SetAffinity { token, .. }
        | Request::SetPriority { token, .. }
        | Request::ApplyRules { token } => token,
    };

    match read_token(rules_dir) {
        Some(expected) if &expected == token => {}
        _ => return Response::error("unauthorized: bridge token mismatch"),
    }

    match request {
        Request::Ping { .. } => Response::ok(),
        Request::SetAffinity { pid, masks, mode, .. } => {
            let masks = match procwin::GroupMasks::from_hex_list(&masks) {
                Ok(masks) => masks.0,
                Err(e) => return Response::error(e),
            };
            match procwin::set_affinity_by_group_masks(pid, &masks, mode) {
                Ok(_) => Response::ok(),
                Err(e) => Response::error(e),
            }
        }
        Request::SetPriority { pid, priority_class, io_priority, memory_priority, .. } => {
            if let Some(value) = priority_class {
                if let Err(e) = procwin::set_process_priority_class(pid, value) {
                    return Response::error(e);
                }
            }
            if let Some(value) = io_priority {
                if let Err(e) = procwin::set_process_io_priority(pid, value) {
                    return Response::error(e);
                }
            }
            if let Some(value) = memory_priority {
                if let Err(e) = procwin::set_process_memory_priority(pid, value) {
                    return Response::error(e);
                }
            }
            let priorities = procwin::get_process_priorities(pid);
            let mut response = Response::ok();
            response.priorities = Some(priorities);
            response
        }
        Request::ApplyRules { .. } => match engine::apply_rules_from_dir(rules_dir) {
            Ok(report) => Response::applied(
                report.applied,
                report.changed.iter().map(AppliedProcess::from).collect(),
            ),
            Err(e) => Response::error(e),
        },
    }
}
