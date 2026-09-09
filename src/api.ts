import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { CpuTopology, ProcessInfo } from "./types";
import { formatMask } from "./types";

export async function getCpuTopology(): Promise<CpuTopology> {
  return await invoke<CpuTopology>("get_cpu_topology");
}

/** 拓扑缓存返回的结构, 签名不对或文件不存在时返回 null (前端回退到真实探测) */
export interface TopologyCache {
  hw_signature: string;
  ts_secs: number;
  topology: CpuTopology;
}

/** 读 CPU 拓扑缓存。签名匹配时立即返回 (<1ms), 否则返回 null。
 *  缓存只对「CPU 没被更换」的场景有效, 换 CPU / 主板时签名自动失效。 */
export async function loadCpuTopologyCache(): Promise<TopologyCache | null> {
  const raw = await invoke<TopologyCache | null>("load_cpu_topology_cache");
  return raw ?? null;
}

/** 写 CPU 拓扑缓存。真实探测结束后调一次, 下次启动就能走 <1ms 快速路径。 */
export async function saveCpuTopologyCache(topology: CpuTopology): Promise<void> {
  await invoke("save_cpu_topology_cache", { topology });
}

/**
 * 首屏全量获取 —— 一次性拉取所有进程的结构字段 + 指标,
 * 并在后端内部写入 metrics 采样 baseline (下一次就能算速率差分)。
 * 只在 onMounted / 手动点击刷新按钮时调用。
 */
export async function listProcesses(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes");
}

/**
 * 超轻量快扫。仅 PID / name / parent_pid, 不 OpenProcess → <20ms 返回。
 * 用于首屏先把进程名/PID 立刻显示出来(避免感知「还在加载」)。
 * 慢字段 (内存/亲和性/CPU 速率) 后续用 listProcessesFull 或 events patch。
 */
export async function listProcessesLight(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes_light");
}

/**
 * 读取上次的进程列表缓存 (<1ms)。首次启动无缓存返回空数组。
 * 用于首屏立即显示上次的进程列表, 避免空白等待。
 */
export async function listProcessesCached(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes_cached");
}

export async function setProcessAffinity(pid: number, mask: bigint): Promise<void> {
  await invoke("set_process_affinity", { pid, mask: formatMask(mask) });
}

// ==========================================================================
// 分步指标推送 (避免前端每 1s 全量拉取 + 整表重建导致的闪烁)
//
// 后端 start_metrics_stream 会每秒采集一次, 并把「每一类指标」分成 4 波
// 通过 Tauri event `process://metrics` 逐步推给前端:
//   wave 1 → pid + cpu_usage_percent
//   wave 2 → pid + memory_bytes
//   wave 3 → pid + disk_read_bps + disk_write_bps
//   wave 4 → pid + net_total_bps
// 波与波之间留有极小间隔 (3~5 ms), 前端就看到「从左到右依次更新」, 而不是整表闪烁。
//
// 单独的 `process://processes-diff` 事件负责把 PID 集合变化 (启动/退出的进程)
// 推给前端, 从而不需要再全量 list_processes 就能保持结构同步。
// ==========================================================================

/** 一波指标推送 (紧凑 tuple 编码以减小序列化体积) */
export interface MetricsWave {
  /** 1..4 波次 */
  wave: 1 | 2 | 3 | 4;
  /**
   * tuple 内容随 wave 不同:
   *   wave 1: [pid, cpu_usage_percent]
   *   wave 2: [pid, memory_bytes]
   *   wave 3: [pid, disk_read_bps, disk_write_bps]
   *   wave 4: [pid, net_total_bps]
   */
  batch: number[][];
}

/** PID 集合有变化时推一次: 全量结构快照 (字段与 ProcessInfo 相同, 但不带速率字段) */
export interface ProcessDiff {
  /** 新增或可能更新 (name/affinity/parent) 的进程 */
  upserts: Array<Omit<ProcessInfo,
    "cpu_usage_percent" | "disk_read_bps" | "disk_write_bps" | "net_total_bps">>;
  /** 已经退出、应该从前端移除的 pid (后端 serialize 成 removed_pids, 这里对齐) */
  removed_pids: number[];
}

/** 亲和性写入成功后后端推一条, 避免前端再全量拉 list_processes */
export interface AffinityUpdated {
  pid: number;
  affinity_mask: string | null;
}

/**
 * 启动后端指标推送流。若已启动则是 no-op。
 * @param intervalMs 采样间隔 (默认 1000 ms)
 */
export async function startMetricsStream(intervalMs = 1000): Promise<void> {
  await invoke("start_metrics_stream", { intervalMs });
}

/** 停止后端指标推送流 (暂停) */
export async function stopMetricsStream(): Promise<void> {
  await invoke("stop_metrics_stream");
}

export const EVENTS = {
  METRICS: "process://metrics" as const,
  PROCESS_DIFF: "process://processes-diff" as const,
  AFFINITY_UPDATED: "process://affinity-updated" as const,
} as const;

/** 一次性注册所有事件监听, 返回统一的 unlisten */
export async function registerProcessListeners(handlers: {
  onMetrics: (m: MetricsWave) => void;
  onProcessDiff: (d: ProcessDiff) => void;
  onAffinityUpdated: (u: AffinityUpdated) => void;
}): Promise<() => void> {
  const unlistens: Array<() => void> = [];

  unlistens.push(await listen<MetricsWave>(EVENTS.METRICS, (ev) => handlers.onMetrics(ev.payload)));
  unlistens.push(
    await listen<ProcessDiff>(EVENTS.PROCESS_DIFF, (ev) => handlers.onProcessDiff(ev.payload)),
  );
  unlistens.push(
    await listen<AffinityUpdated>(EVENTS.AFFINITY_UPDATED, (ev) =>
      handlers.onAffinityUpdated(ev.payload),
    ),
  );

  return () => {
    for (const u of unlistens) u();
  };
}


// ==========================================================================
// 亲和性规则持久化
// ==========================================================================

/** 亲和性规则 */
export interface AffinityRule {
  id: string;
  process_name: string;
  mask: string;
  enabled: boolean;
  created_at: number;
  note: string;
}

/** 保存亲和性规则列表 */
export async function saveAffinityRules(rules: AffinityRule[]): Promise<void> {
  await invoke("save_affinity_rules", { rules });
}

/** 加载亲和性规则列表 */
export async function loadAffinityRules(): Promise<AffinityRule[]> {
  return await invoke<AffinityRule[]>("load_affinity_rules");
}

/** 添加一条亲和性规则 */
export async function addAffinityRule(
  processName: string,
  mask: string,
  note: string = ""
): Promise<AffinityRule> {
  return await invoke<AffinityRule>("add_affinity_rule", { 
    processName, 
    mask, 
    note 
  });
}

/** 更新一条亲和性规则 */
export async function updateAffinityRule(
  id: string,
  updates: {
    processName?: string;
    mask?: string;
    enabled?: boolean;
    note?: string;
  }
): Promise<AffinityRule> {
  return await invoke<AffinityRule>("update_affinity_rule", { 
    id,
    processName: updates.processName,
    mask: updates.mask,
    enabled: updates.enabled,
    note: updates.note,
  });
}

/** 删除一条亲和性规则 */
export async function deleteAffinityRule(id: string): Promise<void> {
  await invoke("delete_affinity_rule", { id });
}

/** 应用所有启用的亲和性规则到当前运行的进程 */
export async function applyAffinityRules(): Promise<number> {
  return await invoke<number>("apply_affinity_rules");
}

/** 生成唯一ID */
export async function generateAffinityRuleId(): Promise<string> {
  return await invoke<string>("generate_affinity_rule_id");
}


// ==========================================================================
// Windows 服务管理 (cpum_service.exe)
// ==========================================================================

/** 服务状态 */
export type ServiceStatus = "running" | "stopped" | "not_installed" | string;

/** 查询服务状态 */
export async function getServiceStatus(): Promise<ServiceStatus> {
  return await invoke<ServiceStatus>("get_service_status");
}

/** 安装服务并设为开机自启（需管理员权限） */
export async function installService(): Promise<string> {
  return await invoke<string>("install_service");
}

/** 卸载服务（需管理员权限） */
export async function uninstallService(): Promise<string> {
  return await invoke<string>("uninstall_service");
}

/** 启动已安装的服务 */
export async function startService(): Promise<string> {
  return await invoke<string>("start_service");
}

/** 停止正在运行的服务 */
export async function stopService(): Promise<string> {
  return await invoke<string>("stop_service");
}

