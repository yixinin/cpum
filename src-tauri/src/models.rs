//! 进程与 CPU 拓扑的共享数据模型 (供 Tauri 命令序列化给前端使用)

use serde::{Deserialize, Serialize};

/// 单个逻辑处理器 (SMT 线程) 的拓扑信息
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LogicalProcessorInfo {
    /// 全局逻辑处理器编号 (单组场景中 = affinity mask 中的 bit 序号)
    pub index: u32,
    /// 所属物理核 ID
    pub core_id: u32,
    /// 所属 CCD / Die ID
    pub die_id: u32,
    /// 所属 CPU 插槽 (Package / Socket) ID
    pub package_id: u32,
    /// 在所属物理核内的 SMT 线程号 (0 表示主线程, 1+ 表示副线程)
    pub smt_thread_id: u32,
    /// Intel hybrid: 0=性能核 (P-core), 1=能效核 (E-core)。其他架构通常为 0
    pub efficiency_class: u8,
    /// 是否为 SMT 副线程 (超线程的第二个逻辑核)
    pub is_smt_secondary: bool,
}

/// 物理核信息
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CoreInfo {
    pub id: u32,
    pub die_id: u32,
    pub package_id: u32,
    /// 该物理核是否启用 SMT (含多个逻辑处理器)
    pub has_smt: bool,
    /// 该物理核包含的逻辑处理器编号列表
    pub threads: Vec<u32>,
    pub efficiency_class: u8,
}

/// CCD / Die 信息 (AMD Zen 架构中一个 CCD = 一个 Die)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DieInfo {
    pub id: u32,
    pub package_id: u32,
    pub cores: Vec<u32>,
    /// 该 CCD 包含的所有逻辑处理器编号 (用于快速选择)
    pub threads: Vec<u32>,
    /// 是否为真正检测到的多 CCD 结构 (false 表示系统未报告 Die 信息, 仅作为占位)
    pub is_ccd: bool,
}

/// 完整的 CPU 拓扑结构
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CpuTopology {
    pub logical_processors: Vec<LogicalProcessorInfo>,
    pub cores: Vec<CoreInfo>,
    pub dies: Vec<DieInfo>,
    /// 系统逻辑处理器总数
    pub total_logical_processors: u32,
    /// 处理器组数量 (仅支持单组场景下使用 u64 mask)
    pub single_group: bool,
}

/// 进程信息 (列表展示用)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// 进程当前 CPU 亲和性 mask (None 表示无法读取, 例如权限不足)
    /// 以十六进制字符串形式传输, 避免大 mask 在 JS 端丢失精度
    pub affinity_mask: Option<String>,
    /// 系统亲和性 mask (所有可用处理器的并集)
    pub system_affinity_mask: Option<String>,
    /// 父进程 PID
    pub parent_pid: u32,
    /// 是否因权限不足无法访问
    pub access_denied: bool,

    // ---------- 资源使用率指标 ----------
    /// 进程 CPU 使用率, 0.0 ~ (逻辑处理器数 * 100.0), 通常单进程 0~100 (即 CPU 满载)
    /// 采样间隔 < 250ms 时返回上一次的值 (避免 0%)
    pub cpu_usage_percent: f32,
    /// 进程当前 Working Set (物理内存工作集), 单位字节
    pub memory_bytes: u64,
    /// 磁盘读速率, 单位 bytes/sec (基于 GetProcessIoCounters 的总 IO 字节, 含 net/管道)
    pub disk_read_bps: u64,
    /// 磁盘写速率, 单位 bytes/sec
    pub disk_write_bps: u64,
    /// 网络下载速率 (BytesIn delta / dt), 单位 bytes/sec。
    /// 基于 NtQueryInformationProcess(ProcessNetworkIoCounters=114), Win11 24H2+ 才有;
    /// 老版本 Windows 此字段恒为 0。
    pub net_in_bps: u64,
    /// 网络上传速率 (BytesOut delta / dt), 单位 bytes/sec。同 net_in_bps。
    pub net_out_bps: u64,
}

/// 将 u64 mask 格式化为带前缀的十六进制字符串 (例如 "0xFFFFFFFF")
pub fn mask_to_hex(mask: u64) -> String {
    format!("0x{:X}", mask)
}

/// 亲和性规则 (持久化保存)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AffinityRule {
    /// 规则ID (UUID)
    pub id: String,
    /// 进程名 (不含.exe后缀)
    pub process_name: String,
    /// 亲和性掩码 (十六进制字符串)
    pub mask: String,
    /// 是否启用
    pub enabled: bool,
    /// 创建时间 (Unix时间戳)
    pub created_at: u64,
    /// 备注
    pub note: String,
}
