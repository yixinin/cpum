import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { CpuTopology, LogicalProcessorUsage, PbLogEntry, PbStatistics, PbStatus, ProcessInfo, ProBalanceConfig, RuleMatchType, RuleMode } from "./types";
import { formatMask } from "./types";

export async function getCpuTopology(): Promise<CpuTopology> {
  return await invoke<CpuTopology>("get_cpu_topology");
}

/** Shape returned by the topology cache; null when the signature mismatches or the file is missing (frontend falls back to a real probe) */
export interface TopologyCache {
  hw_signature: string;
  ts_secs: number;
  topology: CpuTopology;
}

/** Read the CPU topology cache. Returns immediately (<1ms) on a signature match, otherwise null.
 *  The cache is only valid when the CPU hasn't been swapped; a CPU / motherboard change invalidates the signature automatically. */
export async function loadCpuTopologyCache(): Promise<TopologyCache | null> {
  const raw = await invoke<TopologyCache | null>("load_cpu_topology_cache");
  return raw ?? null;
}

/** Write the CPU topology cache. Called once after a real probe, so the next launch can take the <1ms fast path. */
export async function saveCpuTopologyCache(topology: CpuTopology): Promise<void> {
  await invoke("save_cpu_topology_cache", { topology });
}

/**
 * Full first-screen fetch — pulls every process's structural fields and metrics in one call,
 * and also writes the metrics sampling baseline on the backend (so the next call can compute rate deltas).
 * Called only from onMounted / a manual refresh button click.
 */
export async function listProcesses(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes");
}

/**
 * Ultra-lightweight fast scan. Only PID / name / parent_pid, no OpenProcess — returns in <20ms.
 * Used to render the first screen immediately (so the user doesn't perceive a "loading" state).
 * Slow fields (memory / affinity / CPU rate) are filled in later via listProcesses or metrics events.
 */
export async function listProcessesLight(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes_light");
}

/**
 * Read the last cached process list (<1ms). Returns an empty array on first launch.
 * Used to show the previous process list immediately on the first screen, avoiding a blank wait.
 */
export async function listProcessesCached(): Promise<ProcessInfo[]> {
  return await invoke<ProcessInfo[]>("list_processes_cached");
}

/** Full executable path for a single process (for right-click "Copy path" / filling missing paths for new processes). null = no permission / exited */
export async function getProcessExePath(pid: number): Promise<string | null> {
  return await invoke<string | null>("get_process_exe_path", { pid });
}

export async function setProcessAffinity(
  pid: number,
  mask: bigint,
  mode: RuleMode = "strict",
  groupMasks?: string[],
): Promise<void> {
  await invoke("set_process_affinity", { pid, mask: formatMask(mask), mode, groupMasks: groupMasks ?? null });
}

export async function getLogicalProcessorUsage(): Promise<LogicalProcessorUsage[]> {
  return await invoke<LogicalProcessorUsage[]>("get_logical_processor_usage");
}

/**
 * Set a process's priorities (CPU / I/O / memory; only the fields you want to change).
 * On success the backend emits a `process://priority-updated` event so the frontend can patch the row in place.
 */
export async function setProcessPriority(
  pid: number,
  updates: {
    priorityClass?: number;
    ioPriority?: number;
    memoryPriority?: number;
  },
): Promise<void> {
  await invoke("set_process_priority", {
    pid,
    priorityClass: updates.priorityClass ?? null,
    ioPriority: updates.ioPriority ?? null,
    memoryPriority: updates.memoryPriority ?? null,
  });
}

// ==========================================================================
// Staged metrics push (avoids the flicker of pulling + rebuilding the whole table once per second)
//
// The backend's start_metrics_stream samples once per second and splits each
// metric category into 4 waves, pushing them to the frontend through the
// Tauri event `process://metrics`:
//   wave 1 -> pid + cpu_usage_percent
//   wave 2 -> pid + memory_bytes
//   wave 3 -> pid + disk_read_bps + disk_write_bps
//   wave 4 -> pid + net_total_bps
// A small delay (3~5 ms) is inserted between waves so the user sees columns
// update left-to-right instead of the whole table flashing at once.
//
// A separate `process://processes-diff` event delivers PID-set changes
// (process start/exit) so the structural view stays in sync without a
// full list_processes pull.
// ==========================================================================

/** A single metrics wave (compact tuple encoding to shrink the serialized payload) */
export interface MetricsWave {
  /** Wave number 1..4 */
  wave: 1 | 2 | 3 | 4;
  /**
   * Tuple contents depend on the wave:
   *   wave 1: [pid, cpu_usage_percent]
   *   wave 2: [pid, memory_bytes]
   *   wave 3: [pid, disk_read_bps, disk_write_bps]
   *   wave 4: [pid, net_total_bps]
   */
  batch: number[][];
}

/** Pushed once per PID-set change: a full structural snapshot (same fields as ProcessInfo, without the rate fields) */
export interface ProcessDiff {
  /** Newly added (or possibly updated, e.g. name / affinity / parent) processes */
  upserts: Array<Omit<ProcessInfo,
    "cpu_usage_percent" | "disk_read_bps" | "disk_write_bps" | "net_total_bps">>;
  /** PIDs that have exited and should be removed from the frontend (the backend serializes this as removed_pids, matched here) */
  removed_pids: number[];
}

/** Pushed by the backend after a successful affinity write so the frontend doesn't have to re-pull list_processes */
export interface AffinityUpdated {
  pid: number;
  affinity_mask: string | null;
}

/** Pushed by the backend after a successful priority write (with the freshly read values) so the frontend can patch the row in place */
export interface PriorityUpdated {
  pid: number;
  priority_class: number | null;
  io_priority: number | null;
  memory_priority: number | null;
}

/**
 * Start the backend metrics push stream. No-op if already running.
 * @param intervalMs sampling interval (default 1000 ms)
 */
export async function startMetricsStream(intervalMs = 1000): Promise<void> {
  await invoke("start_metrics_stream", { intervalMs });
}

/** Stop the backend metrics push stream (paused) */
export async function stopMetricsStream(): Promise<void> {
  await invoke("stop_metrics_stream");
}

export const EVENTS = {
  METRICS: "process://metrics" as const,
  PROCESS_DIFF: "process://processes-diff" as const,
  AFFINITY_UPDATED: "process://affinity-updated" as const,
  PRIORITY_UPDATED: "process://priority-updated" as const,
} as const;

/** Register all event listeners at once; returns a single unlisten function */
export async function registerProcessListeners(handlers: {
  onMetrics: (m: MetricsWave) => void;
  onProcessDiff: (d: ProcessDiff) => void;
  onAffinityUpdated: (u: AffinityUpdated) => void;
  onPriorityUpdated: (u: PriorityUpdated) => void;
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
  unlistens.push(
    await listen<PriorityUpdated>(EVENTS.PRIORITY_UPDATED, (ev) =>
      handlers.onPriorityUpdated(ev.payload),
    ),
  );

  return () => {
    for (const u of unlistens) u();
  };
}


// ==========================================================================
// Affinity rule persistence
// ==========================================================================

/** Affinity rule (schema v2, aligned with Rust cpum_core::rule::AffinityRule) */
export interface AffinityRule {
  id: string;
  /** Process name / wildcard pattern / path pattern, interpreted per `match_type` */
  process_name: string;
  /** Affinity mask as a hex string (e.g. "0xFF") */
  mask: string;
  /** One hex mask per processor group. Absent means legacy single group mask. */
  group_masks?: string[] | null;
  enabled: boolean;
  created_at: number;
  note: string;
  /** Match mode (new in v2; defaults to "exact" for legacy data) */
  match_type: RuleMatchType;
  /** Scheduling mode: strict = hard mask, soft = CPU Sets (new in v2; defaults to "strict" for legacy data) */
  mode: RuleMode;
  /** CPU priority class managed by this rule (null = not managed) */
  priority_class: number | null;
  /** I/O priority managed by this rule (null = not managed) */
  io_priority: number | null;
  /** Memory priority managed by this rule (null = not managed) */
  memory_priority: number | null;
}

/** Persist the affinity rule list */
export async function saveAffinityRules(rules: AffinityRule[]): Promise<void> {
  await invoke("save_affinity_rules", { rules });
}

/** Load the affinity rule list (legacy v1 files are auto-migrated) */
export async function loadAffinityRules(): Promise<AffinityRule[]> {
  return await invoke<AffinityRule[]>("load_affinity_rules");
}

/** Input for creating a new rule (the backend assigns id / created_at; the rest default to "not set") */
export interface RuleDraft {
  processName: string;
  mask: string;
  groupMasks?: string[];
  note?: string;
  matchType?: RuleMatchType;
  mode?: RuleMode;
  priorityClass?: number | null;
  ioPriority?: number | null;
  memoryPriority?: number | null;
}

/** Add a new affinity rule */
export async function addAffinityRule(draft: RuleDraft): Promise<AffinityRule> {
  return await invoke<AffinityRule>("add_affinity_rule", {
    processName: draft.processName,
    mask: draft.mask,
    groupMasks: draft.groupMasks ?? null,
    note: draft.note ?? null,
    matchType: draft.matchType ?? null,
    mode: draft.mode ?? null,
    priorityClass: draft.priorityClass ?? null,
    ioPriority: draft.ioPriority ?? null,
    memoryPriority: draft.memoryPriority ?? null,
  });
}

/** Update an affinity rule (whole-record replacement, located by rule.id) */
export async function updateAffinityRule(rule: AffinityRule): Promise<AffinityRule> {
  return await invoke<AffinityRule>("update_affinity_rule", { rule });
}

/** Delete an affinity rule */
export async function deleteAffinityRule(id: string): Promise<void> {
  await invoke("delete_affinity_rule", { id });
}

/** Apply every enabled rule to currently running processes (affinity + CPU Sets + priorities) */
export async function applyAffinityRules(): Promise<number> {
  return await invoke<number>("apply_affinity_rules");
}


// ==========================================================================
// ProBalance dynamic-optimization engine (config / status / log)
// ==========================================================================
// Config, status file, and action log all live in a machine-wide directory
// (shared with the service); the service hot-reloads by watching the
// config file's mtime, so saving a config takes effect next second — no restart.

/** Read the ProBalance config (returns the default — disabled — when no file exists yet) */
export async function getProBalanceConfig(): Promise<ProBalanceConfig> {
  return await invoke<ProBalanceConfig>("get_probalance_config");
}

/** Save the ProBalance config (backend-validated; the service picks it up next second) */
export async function saveProBalanceConfig(config: ProBalanceConfig): Promise<void> {
  await invoke("save_probalance_config", { config });
}

/** Read the service-side engine status (null = status file missing, service has never run) */
export async function getProBalanceStatus(): Promise<PbStatus | null> {
  return await invoke<PbStatus | null>("get_probalance_status");
}

/** Read the most recent ProBalance action log entries (downgrade / restore / exit) */
export async function getProBalanceLog(limit = 50): Promise<PbLogEntry[]> {
  return await invoke<PbLogEntry[]>("get_probalance_log", { limit });
}

export async function getProBalanceStatistics(): Promise<PbStatistics> {
  return await invoke<PbStatistics>("get_probalance_statistics");
}


// ==========================================================================
// Windows service management (cpum_service.exe)
// ==========================================================================

/** Service status */
export type ServiceStatus = "running" | "stopped" | "not_installed" | string;

/** Privileged bridge reachability */
export type BridgeStatus = "connected" | "unavailable" | string;

/** Query the service status */
export async function getServiceStatus(): Promise<ServiceStatus> {
  return await invoke<ServiceStatus>("get_service_status");
}

/** Whether the privileged bridge to the LocalSystem service is reachable */
export async function getBridgeStatus(): Promise<BridgeStatus> {
  return await invoke<BridgeStatus>("get_bridge_status");
}

/** Install the service and set it to auto-start (the sc.exe calls are elevated via UAC) */
export async function installService(): Promise<string> {
  return await invoke<string>("install_service");
}

/** Uninstall the service (the sc.exe calls are elevated via UAC) */
export async function uninstallService(): Promise<string> {
  return await invoke<string>("uninstall_service");
}

/** Start an already-installed service */
export async function startService(): Promise<string> {
  return await invoke<string>("start_service");
}

/** Stop a running service */
export async function stopService(): Promise<string> {
  return await invoke<string>("stop_service");
}
