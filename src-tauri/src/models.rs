//! Shared data models for processes and CPU topology (serialized to the
//! frontend via Tauri commands).
use serde::{Deserialize, Serialize};

/// Topology information for a single logical processor (SMT thread).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LogicalProcessorInfo {
    /// Global logical processor index (= bit position in the affinity mask
    /// in the single-group case).
    pub index: u32,
    /// Windows processor group owning this LP.
    pub group: u16,
    /// Bit index within `group` (the bit position used by group_masks).
    pub group_index: u8,
    /// Owning physical core id.
    pub core_id: u32,
    /// Owning CCD / Die id.
    pub die_id: u32,
    /// Owning CPU package (socket) id.
    pub package_id: u32,
    /// SMT thread number within the owning physical core (0 = primary
    /// thread, 1+ = secondary threads).
    pub smt_thread_id: u32,
    /// Intel hybrid: 0 = P-core, 1 = E-core. Other architectures usually 0.
    pub efficiency_class: u8,
    /// Whether this is an SMT secondary thread (the second logical core of
    /// a hyper-threaded core).
    pub is_smt_secondary: bool,
}

/// Physical core information.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CoreInfo {
    pub id: u32,
    pub die_id: u32,
    pub package_id: u32,
    /// Whether SMT is enabled on this physical core (multiple logical
    /// processors).
    pub has_smt: bool,
    /// List of logical processor indices belonging to this physical core.
    pub threads: Vec<u32>,
    pub efficiency_class: u8,
}

/// CCD / Die information (one CCD = one Die on AMD Zen).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DieInfo {
    pub id: u32,
    pub package_id: u32,
    pub cores: Vec<u32>,
    /// All logical processor indices on this CCD (used for fast selection).
    pub threads: Vec<u32>,
    /// Whether this is a genuinely detected multi-CCD structure (false
    /// means the system did not report Die information; the entry is just
    /// a placeholder).
    pub is_ccd: bool,
}

/// Complete CPU topology structure.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CpuTopology {
    pub logical_processors: Vec<LogicalProcessorInfo>,
    pub cores: Vec<CoreInfo>,
    pub dies: Vec<DieInfo>,
    /// Total number of logical processors in the system.
    pub total_logical_processors: u32,
    /// Active Windows processor groups. Values above one require group-aware
    /// affinity and CPU Set handling rather than a single 64-bit mask.
    pub group_count: u16,
    /// Compatibility flag for legacy UI paths.
    pub single_group: bool,
}

/// Process information (used for the process list view).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// Process's current CPU affinity mask (None when unreadable, e.g.
    /// insufficient privileges). Transmitted as a hex string to avoid
    /// precision loss for large masks on the JS side.
    pub affinity_mask: Option<String>,
    /// System affinity mask (union of all available processors).
    pub system_affinity_mask: Option<String>,
    /// One affinity mask per Windows processor group. Present on multi-group
    /// systems; `affinity_mask` remains the group-0 compatibility view.
    pub group_affinity_masks: Option<Vec<String>>,
    /// System-available LPs in each Windows processor group. This is kept
    /// separate from `group_affinity_masks`, which represents the process's
    /// current restriction and must not limit the editor's "Select all".
    #[serde(default)]
    pub group_system_affinity_masks: Option<Vec<String>>,
    /// Parent process PID.
    pub parent_pid: u32,
    /// Whether access is denied (e.g. protected process).
    pub access_denied: bool,

    // ---------- Resource usage metrics ----------
    /// Process CPU usage, 0.0 to (logical_processor_count * 100.0); normally
    /// 0..100 for a single process (CPU fully used). When the sampling
    /// interval is < 250ms, returns the previous value (avoid 0%).
    pub cpu_usage_percent: f32,
    /// Process current Working Set (physical memory), in bytes.
    pub memory_bytes: u64,
    /// Disk read rate, in bytes/sec (based on GetProcessIoCounters' total IO
    /// bytes, including net/pipes).
    pub disk_read_bps: u64,
    /// Disk write rate, in bytes/sec.
    pub disk_write_bps: u64,
    /// Network download rate (BytesIn delta / dt), in bytes/sec. Based on
    /// NtQueryInformationProcess(ProcessNetworkIoCounters=114); only
    /// available on Win11 24H2+. On older Windows versions this field is
    /// always 0.
    pub net_in_bps: u64,
    /// Network upload rate (BytesOut delta / dt), in bytes/sec. Same
    /// caveats as `net_in_bps`.
    pub net_out_bps: u64,
}

/// Format a u64 mask as a prefixed hex string (e.g. "0xFFFFFFFF").
pub fn mask_to_hex(mask: u64) -> String {
    format!("0x{:X}", mask)
}
