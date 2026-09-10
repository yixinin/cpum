import { ref } from "vue";
import type { CpuTopology } from "../types";
import {
  getCpuTopology,
  loadCpuTopologyCache,
  saveCpuTopologyCache,
  type TopologyCache,
} from "../api";

/**
 * CPU topology lifecycle: cache-first load, background refresh, expose derived values.
 */
export function useTopology() {
  const topology = ref<CpuTopology | null>(null);

  const totalLps = () => topology.value?.total_logical_processors ?? 0;
  const cpuBaseline = (mode: "per-core" | "overall" = "per-core") =>
    mode === "overall" ? 100 : totalLps() * 100;

  /** Load from cache (<1ms), then background-refresh from real probe. */
  async function init() {
    try {
      const cached: TopologyCache | null = await loadCpuTopologyCache();
      if (cached?.topology) topology.value = cached.topology;
    } catch { /* ignore */ }

    // Background refresh regardless of cache hit
    try {
      const fresh = await getCpuTopology();
      const t = topology.value;
      const changed =
        !t ||
        t.total_logical_processors !== fresh.total_logical_processors ||
        t.dies.length !== fresh.dies.length ||
        t.cores.length !== fresh.cores.length;
      if (changed) {
        topology.value = fresh;
        saveCpuTopologyCache(fresh).catch(() => {});
      } else {
        topology.value = fresh;
      }
    } catch (e) {
      console.error("getCpuTopology failed:", e);
    }
  }

  return { topology, totalLps, cpuBaseline, init };
}
