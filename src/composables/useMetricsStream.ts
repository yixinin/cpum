import { ref } from "vue";
import type { ProcessInfo, CpuTopology, CpuScaleMode } from "../types";
import {
  formatBps,
  formatMemory,
  scaleCpuPercent,
  formatCpuPercent,
  emptyDisplay,
  refreshDisplayCache,
  refreshCcdBars,
} from "../types";
import {
  startMetricsStream,
  stopMetricsStream,
  registerProcessListeners,
  type MetricsWave,
  type ProcessDiff,
  type AffinityUpdated,
  type PriorityUpdated,
} from "../api";

/**
 * Manages the backend metrics streaming lifecycle and applies
 * incoming metric waves/diffs to the process list in-place.
 */
export function useMetricsStream(opts: {
  processes: () => ProcessInfo[];
  processesByPid: () => Map<number, ProcessInfo>;
  cpuBaseline: () => number;
  cpuScaleMode: () => CpuScaleMode;
  nproc: () => number;
  topology: () => CpuTopology | null;
  onProcessListChanged: () => void;
}) {
  const streaming = ref(true);
  const toggleBusy = ref(false);
  let unlisten: (() => void) | null = null;
  const STREAM_INTERVAL_MS = 1000;

  function applyMetricsWave(wave: MetricsWave) {
    const byPid = opts.processesByPid();
    const baseline = opts.cpuBaseline();
    const mode = opts.cpuScaleMode();
    const np = opts.nproc();
    switch (wave.wave) {
      case 1: {
        for (const row of wave.batch) {
          const target = byPid.get(row[0]);
          if (!target) continue;
          target.cpu_usage_percent = row[1];
          const d = target._display;
          const displayPercent = scaleCpuPercent(row[1], mode, np);
          d.cpu_text = formatCpuPercent(displayPercent);
          if (baseline <= 0) {
            d.cpu_color = "#455a64";
          } else {
            const r = Math.min(1, displayPercent / baseline);
            d.cpu_color =
              r >= 0.75
                ? "#EF5350"
                : r >= 0.4
                  ? "#FFA726"
                  : displayPercent >= 1
                    ? "#66BB6A"
                    : "#455a64";
          }
        }
        break;
      }
      case 2: {
        for (const row of wave.batch) {
          const target = byPid.get(row[0]);
          if (!target) continue;
          target.memory_bytes = row[1];
          target._display.mem_text = row[1] > 0 ? formatMemory(row[1]) : "-";
        }
        break;
      }
      case 3: {
        for (const row of wave.batch) {
          const target = byPid.get(row[0]);
          if (!target) continue;
          target.disk_read_bps = row[1];
          target.disk_write_bps = row[2];
          target.disk_total_bps = row[1] + row[2];
          target._display.disk_r_text = row[1] > 0 ? formatBps(row[1]) : "-";
          target._display.disk_w_text = row[2] > 0 ? formatBps(row[2]) : "-";
        }
        break;
      }
      case 4: {
        for (const row of wave.batch) {
          const target = byPid.get(row[0]);
          if (!target) continue;
          target.net_in_bps = row[1];
          target.net_out_bps = row[2];
          target.net_total_bps = row[1] + row[2];
          target._display.net_in_text = row[1] > 0 ? formatBps(row[1]) : "-";
          target._display.net_out_text = row[2] > 0 ? formatBps(row[2]) : "-";
        }
        break;
      }
    }
  }

  function applyProcessDiff(diff: ProcessDiff) {
    const procs = opts.processes();
    const byPid = opts.processesByPid();
    const baseline = opts.cpuBaseline();
    let changed = false;
    if (diff.removed_pids.length) {
      const removeSet = new Set(diff.removed_pids);
      for (let i = procs.length - 1; i >= 0; i--) {
        if (removeSet.has(procs[i].pid)) {
          procs.splice(i, 1);
        }
      }
      changed = true;
    }
    if (diff.upserts.length) {
      const topo = opts.topology();
      for (const info of diff.upserts) {
        const target = byPid.get(info.pid);
        if (target) {
          const maskChanged = target.affinity_mask !== info.affinity_mask;
          target.name = info.name;
          target.exe_path = info.exe_path;
          target.affinity_mask = info.affinity_mask;
          target.system_affinity_mask = info.system_affinity_mask;
          target.group_affinity_masks = info.group_affinity_masks;
          target.group_system_affinity_masks = info.group_system_affinity_masks;
          target.parent_pid = info.parent_pid;
          target.access_denied = info.access_denied;
          target.memory_bytes = info.memory_bytes;
          target.priority_class = info.priority_class;
          target.io_priority = info.io_priority;
          target.memory_priority = info.memory_priority;
          refreshDisplayCache(target, baseline, { mem: true, mask: maskChanged });
          if (maskChanged) refreshCcdBars(target, topo);
        } else {
          const newItem: ProcessInfo = {
            pid: info.pid,
            name: info.name,
            exe_path: info.exe_path,
            affinity_mask: info.affinity_mask,
            system_affinity_mask: info.system_affinity_mask,
            group_affinity_masks: info.group_affinity_masks,
            group_system_affinity_masks: info.group_system_affinity_masks,
            parent_pid: info.parent_pid,
            access_denied: info.access_denied,
            memory_bytes: info.memory_bytes,
            priority_class: info.priority_class,
            io_priority: info.io_priority,
            memory_priority: info.memory_priority,
            cpu_usage_percent: 0,
            disk_read_bps: 0,
            disk_write_bps: 0,
            disk_total_bps: 0,
            net_in_bps: 0,
            net_out_bps: 0,
            net_total_bps: 0,
            _display: emptyDisplay(),
          };
          refreshDisplayCache(newItem, baseline, undefined, opts.cpuScaleMode(), opts.nproc());
          refreshCcdBars(newItem, topo);
          procs.push(newItem);
        }
      }
      changed = true;
    }
    if (changed) opts.onProcessListChanged();
  }

  function applyAffinityUpdated(u: AffinityUpdated) {
    const byPid = opts.processesByPid();
    const target = byPid.get(u.pid);
    if (!target) return;
    target.affinity_mask = u.affinity_mask;
    refreshDisplayCache(target, opts.cpuBaseline(), { mask: true });
    refreshCcdBars(target, opts.topology());
  }

  function applyPriorityUpdated(u: PriorityUpdated) {
    const byPid = opts.processesByPid();
    const target = byPid.get(u.pid);
    if (!target) return;
    target.priority_class = u.priority_class;
    target.io_priority = u.io_priority;
    target.memory_priority = u.memory_priority;
  }

  async function start() {
    try {
      await startMetricsStream(STREAM_INTERVAL_MS);
      streaming.value = true;
    } catch (e) {
      console.error("startMetricsStream failed:", e);
    }
  }

  async function stop() {
    try {
      await stopMetricsStream();
      streaming.value = false;
    } catch (e) {
      console.error("stopMetricsStream failed:", e);
    }
  }

  async function toggle() {
    toggleBusy.value = true;
    try {
      if (streaming.value) {
        await stop();
      } else {
        await start();
      }
    } finally {
      toggleBusy.value = false;
    }
  }

  async function register() {
    unlisten = await registerProcessListeners({
      onMetrics: applyMetricsWave,
      onProcessDiff: applyProcessDiff,
      onAffinityUpdated: applyAffinityUpdated,
      onPriorityUpdated: applyPriorityUpdated,
    });
  }

  function unregister() {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    streaming,
    toggleBusy,
    start,
    stop,
    toggle,
    register,
    unregister,
  };
}
