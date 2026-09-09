// 与 Rust 端 models.rs 一一对应的前端类型定义
// 注: affinity mask 以十六进制字符串形式传输 (如 "0xFFFFFFFF"),
//      以兼容 64 位全 1 (Threadripper 64+ 线程全选) 的情况。

export interface LogicalProcessorInfo {
  index: number;
  core_id: number;
  die_id: number;
  package_id: number;
  smt_thread_id: number;
  efficiency_class: number;
  is_smt_secondary: boolean;
}

export interface CoreInfo {
  id: number;
  die_id: number;
  package_id: number;
  has_smt: boolean;
  threads: number[];
  efficiency_class: number;
}

export interface DieInfo {
  id: number;
  package_id: number;
  cores: number[];
  threads: number[];
  is_ccd: boolean;
}

export interface CpuTopology {
  logical_processors: LogicalProcessorInfo[];
  cores: CoreInfo[];
  dies: DieInfo[];
  total_logical_processors: number;
  single_group: boolean;
}

export interface CcdBar {
  id: number;
  enabled: number;
  total: number;
  color: string;
}

export interface ProcessDisplay {
  cpu_text: string;
  mem_text: string;
  disk_r_text: string;
  disk_w_text: string;
  /** 网络下载 (BytesIn) 速率文本 — 避免每次渲染都重算 */
  net_in_text: string;
  /** 网络上传 (BytesOut) 速率文本 */
  net_out_text: string;
  /** CPU 颜色 #RRGGBB — 避免每次渲染都重算 */
  cpu_color: string;
  /** 亲和性 Mask 短格式 (超过 12 字符折叠) */
  mask_short: string;
  /** 亲和性 Mask 对应的选中逻辑处理器数 */
  mask_bits: number;
  /** 按 CCD 分组的启用线程数 (亲和性可视化色块) — mask 变化时刷新 */
  ccd_bars: CcdBar[];
}

export interface ProcessInfo {
  pid: number;
  name: string;
  affinity_mask: string | null;
  system_affinity_mask: string | null;
  parent_pid: number;
  access_denied: boolean;
  // ---------- 资源使用率 (后端 delta 采样得到) ----------
  /** CPU 使用率 (0.0 ~ N*100.0, N = 逻辑处理器数) */
  cpu_usage_percent: number;
  /** Working Set 物理内存工作集, 字节 */
  memory_bytes: number;
  /** Disk 读速率, bytes/sec */
  disk_read_bps: number;
  /** Disk 写速率, bytes/sec */
  disk_write_bps: number;
  /**
   * Disk 读写总速率, bytes/sec (前端派生 = read + write)。
   * 仅用于「磁盘」合并列的排序 key; 后端不输出此字段。
   */
  disk_total_bps: number;
  /** 网络下载速率 (BytesIn delta / dt), bytes/sec。Win11 24H2+ 才有, 老版本恒 0 */
  net_in_bps: number;
  /** 网络上传速率 (BytesOut delta / dt), bytes/sec。同 net_in_bps */
  net_out_bps: number;
  /**
   * 网络上下行总速率, bytes/sec (前端派生 = in + out)。
   * 仅用于「网络」合并列的排序 key; 后端不输出此字段。
   */
  net_total_bps: number;
  /** 模板端显示缓存 — 由 refreshDisplayCache() 维护, 避免每帧重复格式化 */
  _display: ProcessDisplay;
}

// ---------- 使用率格式化工具 ----------

/** 把 bytes/sec 格式化为 KB/s / MB/s / GB/s 自适应 */
export function formatBps(bps: number): string {
  if (!Number.isFinite(bps) || bps <= 0) return "0 B/s";
  const units = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
  let i = 0;
  let v = bps;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = i === 0 ? 0 : i <= 1 ? 1 : 2;
  return `${v.toFixed(digits)} ${units[i]}`;
}

/** 把 bytes 内存大小格式化为 KB/MB/GB */
export function formatMemory(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = bytes;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = i === 0 ? 0 : i <= 1 ? 1 : 2;
  return `${v.toFixed(digits)} ${units[i]}`;
}

// ---------- mask 工具函数 ----------

/** 将十六进制字符串解析为 bigint */
export function parseMask(hex: string | null): bigint {
  if (!hex) return 0n;
  // 兼容 "0x..." 前缀
  const s = hex.trim().replace(/^0x/i, "");
  if (s === "") return 0n;
  return BigInt("0x" + s);
}

/** 将 bigint 格式化为带前缀的十六进制字符串 */
export function formatMask(mask: bigint): string {
  return "0x" + mask.toString(16).toUpperCase();
}

/** 统计 mask 中置位的位数 (启用的逻辑处理器数) */
export function popcount(mask: bigint): number {
  let n = 0n;
  let m = mask;
  while (m > 0n) {
    n += m & 1n;
    m >>= 1n;
  }
  return Number(n);
}

/** 读取 mask 指定位 (bit 索引从 0 起) 的状态 */
export function getBit(mask: bigint, bit: number): boolean {
  return (mask & (1n << BigInt(bit))) !== 0n;
}

/** 置位 / 清除 mask 的指定位 */
export function setBit(mask: bigint, bit: number, value: boolean): bigint {
  const b = 1n << BigInt(bit);
  return value ? (mask | b) : (mask & ~b);
}

// ---------- ProcessDisplay 缓存工具 (避免模板每帧重格式化) ----------

export function emptyDisplay(): ProcessDisplay {
  return {
    cpu_text: "0%",
    mem_text: "-",
    disk_r_text: "-",
    disk_w_text: "-",
    net_in_text: "-",
    net_out_text: "-",
    cpu_color: "#455a64",
    mask_short: "-",
    mask_bits: 0,
    ccd_bars: [],
  };
}

/** CCD 色块配色 (与 App.vue CCD_TABLE_COLORS 保持一致) */
export const CCD_COLORS = ["#42A5F5", "#66BB6A", "#FFA726", "#EF5350", "#AB47BC", "#26C6DA", "#FFEE58", "#8D6E63"];

/**
 * 刷新 _display.ccd_bars: 按 topology.dies 分组统计 mask 中启用的线程数。
 * 只在 mask 变化 或 topology 变化 时调用 (而不是每帧渲染)。
 */
export function refreshCcdBars(p: ProcessInfo, topology: CpuTopology | null) {
  if (!topology) {
    p._display.ccd_bars = [];
    return;
  }
  const mask = parseMask(p.affinity_mask);
  p._display.ccd_bars = topology.dies.map((die, idx) => ({
    id: die.id,
    enabled: die.threads.filter((t) => (mask & (1n << BigInt(t))) !== 0n).length,
    total: die.threads.length,
    color: CCD_COLORS[die.id % CCD_COLORS.length],
    idx,
  }));
}

/** CPU 百分比显示基准: per-core = 单核 100%, overall = 整体 CPU 100% (与任务管理器右键切换一致) */
export type CpuScaleMode = "per-core" | "overall";

/** 把 CPU 百分比格式化为 1 位小数, 无小数时取整 */
export function formatCpuPercent(p: number): string {
  if (!Number.isFinite(p) || p <= 0) return "0%";
  if (p < 1) return "<1%";
  return `${p.toFixed(p % 1 === 0 ? 0 : 1)}%`;
}

/**
 * 根据 mode 把后端原始 cpu_percent (单核基准) 归一化为显示值。
 * - per-core: 原值返回 (100% = 单核满载)
 * - overall:  除以 nproc (100% = 全部 LP 满载)
 */
export function scaleCpuPercent(p: number, mode: CpuScaleMode, nproc: number): number {
  if (mode === "overall" && nproc > 1) return p / nproc;
  return p;
}

/** 计算 CPU 颜色: 低灰/绿/橙/红 */
export function cpuColor(p: number, baseline: number): string {
  const ratio = Math.min(1, baseline > 0 ? p / baseline : 0);
  if (ratio >= 0.75) return "#EF5350";
  if (ratio >= 0.4) return "#FFA726";
  if (p >= 1) return "#66BB6A";
  return "#455a64";
}

/**
 * 就地刷新 ProcessInfo._display 缓存。
 * 只在 metrics 写入 / 进程 diff / 亲和性更新 后对受影响对象调用一次,
 * 而不是在模板的每个 cell 中 每帧 计算。
 *
 * @param baseline CPU 使用率基线 = 逻辑处理器数 × 100 (per-core) 或 100 (overall)
 * @param cpuMode CPU 显示基准模式, 默认 per-core
 * @param nproc 逻辑处理器数, 用于 overall 模式归一化
 */
export function refreshDisplayCache(
  p: ProcessInfo,
  baseline: number,
  opts: { cpu?: boolean; mem?: boolean; disk?: boolean; net?: boolean; mask?: boolean } = {
    cpu: true,
    mem: true,
    disk: true,
    net: true,
    mask: true,
  },
  cpuMode: CpuScaleMode = "per-core",
  nproc: number = 1,
) {
  const d = p._display;
  if (opts.cpu) {
    const displayPercent = scaleCpuPercent(p.cpu_usage_percent, cpuMode, nproc);
    d.cpu_text = formatCpuPercent(displayPercent);
    d.cpu_color = cpuColor(displayPercent, baseline);
  }
  if (opts.mem) {
    d.mem_text = p.memory_bytes > 0 ? formatMemory(p.memory_bytes) : "-";
  }
  if (opts.disk) {
    d.disk_r_text = p.disk_read_bps > 0 ? formatBps(p.disk_read_bps) : "-";
    d.disk_w_text = p.disk_write_bps > 0 ? formatBps(p.disk_write_bps) : "-";
  }
  if (opts.net) {
    d.net_in_text = p.net_in_bps > 0 ? formatBps(p.net_in_bps) : "-";
    d.net_out_text = p.net_out_bps > 0 ? formatBps(p.net_out_bps) : "-";
  }
  if (opts.mask) {
    const mask = p.affinity_mask;
    if (!mask) {
      d.mask_short = "-";
      d.mask_bits = 0;
    } else {
      d.mask_short = mask.length > 12 ? mask.slice(0, 6) + "…" + mask.slice(-4) : mask;
      d.mask_bits = popcount(parseMask(mask));
    }
  }
}
