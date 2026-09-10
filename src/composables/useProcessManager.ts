import { ref, computed } from "vue";
import type { ProcessInfo, CpuTopology, CpuScaleMode } from "../types";
import { emptyDisplay, refreshDisplayCache, refreshCcdBars, parseMask } from "../types";
import { listProcesses, listProcessesLight, listProcessesCached, setProcessAffinity, applyAffinityRules } from "../api";
import { t } from "../i18n";

/**
 * Process list lifecycle: bootstrap (cached -> light -> full), refresh, search filter.
 */
export function useProcessManager(opts: {
  topology: () => CpuTopology | null;
  cpuBaseline: () => number;
  cpuScaleMode: () => CpuScaleMode;
  nproc: () => number;
  showSnack: (msg: string, color?: "success" | "error" | "info") => void;
}) {
  const processes = ref<ProcessInfo[]>([]);
  const loading = ref(false);
  const search = ref("");

  const processesByPid = computed(() => {
    const m = new Map<number, ProcessInfo>();
    for (const p of processes.value) m.set(p.pid, p);
    return m;
  });

  function initDisplay(p: ProcessInfo) {
    if (!p._display) p._display = emptyDisplay();
    refreshDisplayCache(p, opts.cpuBaseline(), undefined, opts.cpuScaleMode(), opts.nproc());
    refreshCcdBars(p, opts.topology());
  }

  /** 3-stage bootstrap: cached -> light -> full (each independently try-caught). */
  async function bootstrap() {

    // Stage 1: cached (<1ms)
    try {
      const cached = await listProcessesCached();
      if (cached.length > 0) {
        for (const p of cached) initDisplay(p);
        processes.value = cached;
      }
    } catch { /* continue */ }

    // Stage 2: light (<20ms, PID/name only)
    try {
      const light = await listProcessesLight();
      if (light.length > 0) {
        for (const p of light) initDisplay(p);
        processes.value = light;
      }
    } catch { /* continue */ }

    // Stage 3: full (with metrics baseline)
    await refreshFull();
  }

  /** Full refresh with loading indicator. */
  async function refreshFull() {
    loading.value = true;
    try {
      const result = await listProcesses();
      for (const p of result) initDisplay(p);
      processes.value = result;
    } catch (e) {
      opts.showSnack(t("loadProcessesFailed", { error: String(e) }), "error");
    } finally {
      loading.value = false;
    }
  }

  /** Reset a process to system default affinity. */
  async function resetAffinity(p: ProcessInfo) {
    const sysMask = parseMask(p.system_affinity_mask);
    if (sysMask === 0n) {
      opts.showSnack(t("resetNoSysMask"), "error");
      return;
    }
    try {
      await setProcessAffinity(p.pid, sysMask);
      opts.showSnack(t("resetDone", { name: p.name, pid: p.pid }), "success");
    } catch (e) {
      opts.showSnack(t("resetFailed", { error: String(e) }), "error");
    }
  }

  /** Apply all enabled affinity rules, then refresh. */
  async function applyRules() {
    try {
      const count = await applyAffinityRules();
      if (count > 0) {
        opts.showSnack(t("rulesAppliedTo", { count }), "success");
        await refreshFull();
      }
    } catch (e) {
      opts.showSnack(t("applyRulesFailed", { error: String(e) }), "error");
    }
  }

  return {
    processes,
    processesByPid,
    loading,
    search,
    bootstrap,
    refreshFull,
    resetAffinity,
    applyRules,
    initDisplay,
  };
}
