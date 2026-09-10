// Frontend type definitions mirroring Rust models.rs one-to-one.
// Note: affinity masks are transported as hex strings (e.g. "0xFFFFFFFF")
//       so 64-bit all-ones (Threadripper with 64+ threads, all selected) round-trip cleanly.

import { t } from "./i18n";

export interface LogicalProcessorInfo {
  index: number;
  group: number;
  group_index: number;
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
  group_count: number;
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
  /** Pre-formatted download rate text (BytesIn) — avoids re-formatting every render */
  net_in_text: string;
  /** Pre-formatted upload rate text (BytesOut) */
  net_out_text: string;
  /** CPU color #RRGGBB — pre-computed, avoids re-computing every render */
  cpu_color: string;
  /** Short form of the affinity mask (folded when longer than 12 chars) */
  mask_short: string;
  /** Number of selected logical processors corresponding to the affinity mask */
  mask_bits: number;
  /** Enabled-thread count grouped by CCD (used for the affinity visualization swatches); refreshes on mask change */
  ccd_bars: CcdBar[];
}

export interface ProcessInfo {
  pid: number;
  name: string;
  /** Full executable path (Win32 path; null for fast-skip / protected processes) */
  exe_path: string | null;
  affinity_mask: string | null;
  system_affinity_mask: string | null;
  group_affinity_masks: string[] | null;
  /** System-available LPs in each processor group (not the process's current mask). */
  group_system_affinity_masks: string[] | null;
  parent_pid: number;
  access_denied: boolean;
  // ---------- Process priorities (aligned with Rust models.rs) ----------
  /** CPU priority class raw value (0x20=Normal, 0x4000=BelowNormal, ...); null = unreadable */
  priority_class: number | null;
  /** I/O priority (0=VeryLow, 1=Low, 2=Normal); null = unreadable */
  io_priority: number | null;
  /** Memory priority (1=VeryLow ... 5=Normal); null = unreadable */
  memory_priority: number | null;
  // ---------- Resource usage (computed from backend delta sampling) ----------
  /** CPU usage in 0.0 .. N*100.0 (N = number of logical processors) */
  cpu_usage_percent: number;
  /** Working-set physical memory, in bytes */
  memory_bytes: number;
  /** Disk read rate, bytes/sec */
  disk_read_bps: number;
  /** Disk write rate, bytes/sec */
  disk_write_bps: number;
  /**
   * Total disk read+write rate, bytes/sec (frontend-derived = read + write).
   * Used only as the sort key for the merged "Disk" column; the backend does not emit this field.
   */
  disk_total_bps: number;
  /** Net download rate (BytesIn delta / dt), bytes/sec. Available on Win11 24H2+; always 0 on older releases */
  net_in_bps: number;
  /** Net upload rate (BytesOut delta / dt), bytes/sec. Same availability as net_in_bps */
  net_out_bps: number;
  /**
   * Total net up+down rate, bytes/sec (frontend-derived = in + out).
   * Used only as the sort key for the merged "Net" column; the backend does not emit this field.
   */
  net_total_bps: number;
  /** Per-process template-side display cache — maintained by refreshDisplayCache() to avoid re-formatting every frame */
  _display: ProcessDisplay;
}

// ---------- Usage formatting utilities ----------

/** Format bytes/sec with adaptive KB/s / MB/s / GB/s units */
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

/** Format a memory byte count as KB / MB / GB */
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

// ---------- Mask utility functions ----------

/** Parse a hex string (with optional 0x prefix) into a bigint */
export function parseMask(hex: string | null): bigint {
  if (!hex) return 0n;
  // Tolerate an optional "0x" prefix
  const s = hex.trim().replace(/^0x/i, "");
  if (s === "") return 0n;
  return BigInt("0x" + s);
}

/** Format a bigint as a prefixed hex string */
export function formatMask(mask: bigint): string {
  return "0x" + mask.toString(16).toUpperCase();
}

/** Count the set bits in a mask (= number of enabled logical processors) */
export function popcount(mask: bigint): number {
  let n = 0n;
  let m = mask;
  while (m > 0n) {
    n += m & 1n;
    m >>= 1n;
  }
  return Number(n);
}

/** Read a single bit from the mask (bit index starts at 0) */
export function getBit(mask: bigint, bit: number): boolean {
  return (mask & (1n << BigInt(bit))) !== 0n;
}

/** Set or clear a single bit in the mask */
export function setBit(mask: bigint, bit: number, value: boolean): bigint {
  const b = 1n << BigInt(bit);
  return value ? (mask | b) : (mask & ~b);
}

// ---------- Rule types (aligned with Rust cpum_core::rule) ----------

/** Rule match mode: exact = exact name, wildcard = glob, path = full path */
export type RuleMatchType = "exact" | "wildcard" | "path";

/** Scheduling mode: strict = hard mask, soft = elastic CPU Sets */
export type RuleMode = "strict" | "soft";

/** Match-mode options (labelKey points at a key in i18n.ts) */
export const MATCH_TYPE_OPTIONS: Array<{ value: RuleMatchType; labelKey: string }> = [
  { value: "exact", labelKey: "matchExact" },
  { value: "wildcard", labelKey: "matchWildcard" },
  { value: "path", labelKey: "matchPath" },
];

/** Scheduling-mode options */
export const RULE_MODE_OPTIONS: Array<{ value: RuleMode; labelKey: string }> = [
  { value: "strict", labelKey: "modeStrict" },
  { value: "soft", labelKey: "modeSoft" },
];

/** CPU priority class raw values (Win32 PROCESS_CREATION_FLAGS) */
export const PRIORITY_CLASS = {
  IDLE: 0x40,
  BELOW_NORMAL: 0x4000,
  NORMAL: 0x20,
  ABOVE_NORMAL: 0x8000,
  HIGH: 0x80,
  REALTIME: 0x100,
} as const;

/** CPU priority tiers (high to low); labelKey points at a key in i18n.ts */
export const PRIORITY_CLASS_OPTIONS: Array<{ value: number; labelKey: string }> = [
  { value: PRIORITY_CLASS.REALTIME, labelKey: "prioRealtime" },
  { value: PRIORITY_CLASS.HIGH, labelKey: "prioHigh" },
  { value: PRIORITY_CLASS.ABOVE_NORMAL, labelKey: "prioAboveNormal" },
  { value: PRIORITY_CLASS.NORMAL, labelKey: "prioNormal" },
  { value: PRIORITY_CLASS.BELOW_NORMAL, labelKey: "prioBelowNormal" },
  { value: PRIORITY_CLASS.IDLE, labelKey: "prioIdle" },
];

/** I/O priority tiers (3=High is reserved for the system and not exposed) */
export const IO_PRIORITY_OPTIONS: Array<{ value: number; labelKey: string }> = [
  { value: 2, labelKey: "prioNormal" },
  { value: 1, labelKey: "prioLow" },
  { value: 0, labelKey: "prioVeryLow" },
];

/** Memory priority tiers (Win32 MEMORY_PRIORITY: 1=VeryLow ... 5=Normal) */
export const MEMORY_PRIORITY_OPTIONS: Array<{ value: number; labelKey: string }> = [
  { value: 5, labelKey: "prioNormal" },
  { value: 4, labelKey: "prioBelowNormal" },
  { value: 3, labelKey: "prioMedium" },
  { value: 2, labelKey: "prioLow" },
  { value: 1, labelKey: "prioVeryLow" },
];

/** Color for non-Normal CPU priority tiers (used in the process list column) */
export const PRIORITY_CLASS_COLORS: Record<number, string> = {
  [PRIORITY_CLASS.REALTIME]: "#EF5350",
  [PRIORITY_CLASS.HIGH]: "#FFA726",
  [PRIORITY_CLASS.ABOVE_NORMAL]: "#FFB74D",
  [PRIORITY_CLASS.BELOW_NORMAL]: "#42A5F5",
  [PRIORITY_CLASS.IDLE]: "#90A4AE",
};

// ---------- Priority display utilities ----------
// Labels are computed at render time (a lightweight switch), stay reactive
// to locale changes, and are intentionally not cached in _display.

/** CPU priority class raw value -> short label */
export function priorityClassLabel(pc: number | null): string {
  if (pc === null) return "-";
  switch (pc) {
    case PRIORITY_CLASS.REALTIME: return t("prioRealtime");
    case PRIORITY_CLASS.HIGH: return t("prioHigh");
    case PRIORITY_CLASS.ABOVE_NORMAL: return t("prioAboveNormal");
    case PRIORITY_CLASS.NORMAL: return t("prioNormal");
    case PRIORITY_CLASS.BELOW_NORMAL: return t("prioBelowNormal");
    case PRIORITY_CLASS.IDLE: return t("prioIdle");
    default: return `0x${pc.toString(16).toUpperCase()}`;
  }
}

/** I/O priority -> short label */
export function ioPriorityLabel(io: number | null): string {
  if (io === null) return "-";
  if (io === 2) return t("prioNormal");
  if (io === 1) return t("prioLow");
  if (io === 0) return t("prioVeryLow");
  return String(io);
}

/** Memory priority -> short label */
export function memoryPriorityLabel(mp: number | null): string {
  if (mp === null) return "-";
  if (mp === 5) return t("prioNormal");
  if (mp === 4) return t("prioBelowNormal");
  if (mp === 3) return t("prioMedium");
  if (mp === 2) return t("prioLow");
  if (mp === 1) return t("prioVeryLow");
  return String(mp);
}

/** Returns the label color for non-Normal CPU priority tiers; empty string for Normal / unknown */
export function priorityClassColor(pc: number | null): string {
  if (pc === null || pc === PRIORITY_CLASS.NORMAL) return "";
  return PRIORITY_CLASS_COLORS[pc] ?? "";
}

// ---------- ProcessDisplay cache utilities (avoids re-formatting every template frame) ----------

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

/** CCD swatch palette (kept in sync with App.vue CCD_TABLE_COLORS) */
export const CCD_COLORS = ["#42A5F5", "#66BB6A", "#FFA726", "#EF5350", "#AB47BC", "#26C6DA", "#FFEE58", "#8D6E63"];

/**
 * Refresh _display.ccd_bars: per-die enabled-thread counts derived from the mask.
 * Call only on mask change or topology change — not every frame.
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

/** CPU percent scale: per-core = 100% per single core, overall = 100% across all CPUs (matches Task Manager right-click toggle) */
export type CpuScaleMode = "per-core" | "overall";

/** Format a CPU percent to 1 decimal place, dropping the decimal when it's whole */
export function formatCpuPercent(p: number): string {
  if (!Number.isFinite(p) || p <= 0) return "0%";
  if (p < 1) return "<1%";
  return `${p.toFixed(p % 1 === 0 ? 0 : 1)}%`;
}

/**
 * Normalize the backend's raw (per-core baseline) cpu_percent to a display value for the given mode.
 * - per-core: returned as-is (100% = one core saturated)
 * - overall:  divided by nproc (100% = all LPs saturated)
 */
export function scaleCpuPercent(p: number, mode: CpuScaleMode, nproc: number): number {
  if (mode === "overall" && nproc > 1) return p / nproc;
  return p;
}

/** CPU color: low = gray, mid-low = green, mid = orange, high = red */
export function cpuColor(p: number, baseline: number): string {
  const ratio = Math.min(1, baseline > 0 ? p / baseline : 0);
  if (ratio >= 0.75) return "#EF5350";
  if (ratio >= 0.4) return "#FFA726";
  if (p >= 1) return "#66BB6A";
  return "#455a64";
}

/**
 * In-place refresh of ProcessInfo._display cache.
 * Call this once per affected process after a metrics write / process diff /
 * affinity update — never per template cell, per frame.
 *
 * @param baseline CPU usage baseline = logical processor count * 100 (per-core) or 100 (overall)
 * @param opts which fields to refresh (defaults: all)
 * @param cpuMode CPU display mode; defaults to per-core
 * @param nproc logical processor count, used to normalize in overall mode
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

// ==========================================================================
// ProBalance dynamic-optimization engine (aligned with Rust cpum_core::probalance)
// ==========================================================================

/** ProBalance config (edited by the GUI / hot-reloaded by the service; persisted as probalance.json) */
export interface ProBalanceConfig {
  /** Master switch (off = engine idle; any downgraded processes are restored immediately) */
  enabled: boolean;
  /** Foreground-process CPU trigger threshold (per-core baseline %; 100 = one core fully consumed) */
  fg_cpu_threshold: number;
  /** Background-process CPU downgrade threshold (per-core baseline %) */
  bg_cpu_threshold: number;
  /** Contention must persist for this many seconds (debounce against transient spikes) */
  sustain_secs: number;
  /** Seconds to wait after contention clears before restoring (hysteresis, prevents flapping) */
  restore_after_secs: number;
  /** Per-process max downgrade duration (seconds); auto-restore on timeout (safety net) */
  max_downgrade_secs: number;
  /** User-supplied allowlist (supports wildcards; system-critical processes are already hard-coded) */
  whitelist: string[];
  /** Opt-in fullscreen policy: boost the verified fullscreen foreground process and reuse ProBalance for background suppression. */
  game_mode_enabled: boolean;
}

/** Snapshot of the three priority classes (aligned with Rust ProcessPriorities; null = read failed / unsupported) */
export interface PbPriorities {
  priority_class: number | null;
  io_priority: number | null;
  memory_priority: number | null;
}

/** Real-time engine status from the service (overwrites the status file every second; ts lets the GUI liveness-check the service) */
export interface PbStatus {
  /** Write time (Unix seconds) — the GUI uses freshness to determine whether the service is alive */
  ts: number;
  enabled: boolean;
  /** Whether the engine is currently in a downgrade state (contention ongoing) */
  engaged: boolean;
  /** Number of processes currently downgraded */
  downgraded: number;
  fg_pid: number | null;
  fg_cpu_percent: number | null;
  game_mode_active: boolean;
}

/** A single ProBalance action log entry (downgrade / restore / exit) */
export interface PbLogEntry {
  /** Unix seconds */
  ts: number;
  /** "downgrade" | "restore" | "exit" */
  action: string;
  /** Restore reason: contention_cleared / timeout / process_exited / disabled / shutdown */
  reason: string | null;
  pid: number;
  name: string;
  /** CPU usage at downgrade time (per-core baseline %) */
  cpu: number | null;
  from: PbPriorities | null;
  to: PbPriorities | null;
}

export interface LogicalProcessorUsage {
  index: number;
  usage_percent: number;
}

export interface PbStatistics {
  downgrade_count: number;
  restore_count: number;
  exit_count: number;
  unique_processes: number;
}
