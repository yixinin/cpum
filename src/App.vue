<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, nextTick, watch } from "vue";
import type { ProcessInfo, CpuScaleMode } from "./types";
import { refreshDisplayCache, formatMemory, priorityClassLabel, ioPriorityLabel, memoryPriorityLabel, priorityClassColor } from "./types";
import { getServiceStatus, getProcessExePath, getLogicalProcessorUsage, installService, uninstallService, startService, stopService, type ServiceStatus, type AffinityRule } from "./api";
import type { LogicalProcessorUsage } from "./types";
import type { SortItem, ViewMode } from "./constants";
import { useTopology } from "./composables/useTopology";
import { useProcessManager } from "./composables/useProcessManager";
import { useMetricsStream } from "./composables/useMetricsStream";
import { buildProcessTree, flattenTree, computeSearchWhitelist, filterTree, countChildren, toggleTreeNodeExpand, type TableRow } from "./composables/useProcessTree";
import AffinityEditor from "./components/AffinityEditor.vue";
import AffinityRuleManager from "./components/AffinityRuleManager.vue";
import ProBalancePanel from "./components/ProBalancePanel.vue";
import { useI18n } from "./i18n";
import { useTheme } from "./composables/useTheme";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { getVersion } from "@tauri-apps/api/app";
const { t, toggleLocale } = useI18n();
const { theme, toggleTheme } = useTheme();
const updateLoading = ref(false);
const appVersion = ref("");
const appVersionLabel = computed(() => appVersion.value || t("unknownVersion"));

// Theme button: the icon / tooltip show the theme the user will switch TO, not the current one
const themeIcon = computed(() => (theme.value === "dark" ? "mdi-weather-sunny" : "mdi-weather-night"));
const themeTooltip = computed(() => (theme.value === "dark" ? t("themeLight") : t("themeDark")));

// ---------- Composables ----------
const { topology, totalLps, cpuBaseline, init: initTopology } = useTopology();
const nproc = computed(() => totalLps());
const cpuScaleMode = ref<CpuScaleMode>("per-core");
const getCpuBaseline = () => cpuBaseline(cpuScaleMode.value);

const {
  processes, processesByPid, loading, search,
  bootstrap, refreshFull, resetAffinity, applyRules,
} = useProcessManager({
  topology: () => topology.value,
  cpuBaseline: getCpuBaseline,
  cpuScaleMode: () => cpuScaleMode.value,
  nproc: () => nproc.value,
  showSnack,
});

const metricsStream = useMetricsStream({
  processes: () => processes.value,
  processesByPid: () => processesByPid.value,
  cpuBaseline: getCpuBaseline,
  cpuScaleMode: () => cpuScaleMode.value,
  nproc: () => nproc.value,
  topology: () => topology.value,
  onProcessListChanged: () => rebuildProcessesByPid(),
});

// ---------- State ----------
const viewMode = ref<ViewMode>("flat");
const expandedPids = ref<number[]>([]);
const sortBy = ref<readonly SortItem[]>([{ key: "cpu_usage_percent", order: "desc" }]);
const tableCardRef = ref<HTMLElement | null>(null);
const tableHeight = ref(480);
const logicalProcessorUsage = ref<LogicalProcessorUsage[]>([]);
let coreUsageTimer: ReturnType<typeof setInterval> | null = null;

async function refreshLogicalProcessorUsage() {
  try { logicalProcessorUsage.value = await getLogicalProcessorUsage(); } catch { /* unsupported or transient failure */ }
}

// CPU core usage card: keep each visible row's core count as even as
// possible. Pick the fewest rows R such that ceil(total / R) still fits
// the available width; per-row count is then derived from R so the
// remainder lands in a single (last) row instead of being scattered.
// Without this, the naive "pack as many per row as fit" approach
// produces uneven splits (e.g. 14 cores on a 8-col row -> 8 / 6).
const cpuCoresCardRef = ref<HTMLElement | null>(null);
const coresPerRow = ref(16);
let coresResizeObserver: ResizeObserver | null = null;

// Per .core-usage cell width (54px from styles + 8px ga-2 gap). Subtract
// the card's pa-2 padding (8px each side) before dividing.
const CORE_CELL_PITCH = 62;
const CORE_CARD_PADDING = 16;

function updateCoresPerRow() {
  const el = cpuCoresCardRef.value as HTMLElement | null;
  if (!el) return;
  // Defensive: refs on Vuetify components resolve to component instances
  // rather than DOM elements; fall back to a sensible default rather than
  // letting NaN propagate into coreRows (which would yield an empty slice
  // and hide every core).
  const cardWidth = el.clientWidth;
  if (typeof cardWidth !== "number" || !Number.isFinite(cardWidth) || cardWidth <= 0) return;
  const available = Math.max(0, cardWidth - CORE_CARD_PADDING);
  const total = logicalProcessorUsage.value.length;
  if (total <= 0) {
    coresPerRow.value = 0;
    return;
  }
  const maxCols = Math.max(1, Math.floor(available / CORE_CELL_PITCH));
  // Smallest row count R such that ceil(total / R) <= maxCols. Per-row
  // count = ceil(total / R); chunking then gives rows of that size, with
  // the last row carrying the (total % cols) remainder.
  let bestR = total;
  for (let r = 1; r <= total; r++) {
    if (Math.ceil(total / r) <= maxCols) {
      bestR = r;
      break;
    }
  }
  coresPerRow.value = Math.ceil(total / bestR);
}

const coreRows = computed<LogicalProcessorUsage[][]>(() => {
  const arr = logicalProcessorUsage.value;
  if (arr.length === 0) return [];
  const cols = Math.max(1, coresPerRow.value);
  const rows: LogicalProcessorUsage[][] = [];
  for (let i = 0; i < arr.length; i += cols) {
    rows.push(arr.slice(i, i + cols));
  }
  return rows;
});

// Watch the ref so the observer is re-attached if the v-card is
// unmounted/remounted (v-if gates the card on data availability).
watch(cpuCoresCardRef, (el, _prev, onCleanup) => {
  coresResizeObserver?.disconnect();
  coresResizeObserver = null;
  if (!el) return;
  nextTick(() => updateCoresPerRow());
  if (typeof ResizeObserver !== "undefined") {
    coresResizeObserver = new ResizeObserver(() => updateCoresPerRow());
    coresResizeObserver.observe(el);
  }
  onCleanup(() => {
    coresResizeObserver?.disconnect();
    coresResizeObserver = null;
  });
});

// Affinity editor — driven by a discriminated-union `target` so the same
// component powers both the process-list right-click "Edit Rule" and the
// rule manager's Add / Edit actions (target.kind switches the behavior).
type EditorTarget =
  | { kind: "process"; process: ProcessInfo }
  | { kind: "rule"; rule: AffinityRule; isNew: boolean };
const editorOpen = ref(false);
const editorTarget = ref<EditorTarget | null>(null);

// Rule manager
const ruleManagerOpen = ref(false);
// ProBalance dynamic optimization panel
const pbPanelOpen = ref(false);
// Service management
const serviceDialogOpen = ref(false);
const serviceStatus = ref<ServiceStatus>("not_installed");
const serviceLoading = ref(false);
const serviceMessage = ref("");

// Snackbar
const snackbar = ref(false);
const snackbarText = ref("");
const snackbarColor = ref<"success" | "error" | "info">("info");

// Context menu
const ctxMenu = ref({ open: false, x: 0, y: 0, type: null as "processRow" | "cpuInfo" | null, process: null as ProcessInfo | null });

// CPU scale
const cpuScaleIcon = computed(() => cpuScaleMode.value === "overall" ? "mdi-cpu-64-bit" : "mdi-chip");
const cpuScaleTooltip = computed(() => cpuScaleMode.value === "overall" ? t("cpuScaleOverall") : t("cpuScalePerCore"));

// CPU info bar: die-group badge (multi-die highlighted; labeled "CCD" vs "Die" per die.is_ccd)
const isMultiDie = computed(() => (topology.value?.dies.length ?? 0) > 1);
const dieGroupLabel = computed(() => {
  const dies = topology.value?.dies;
  if (!dies || dies.length === 0) return "";
  return dies.some((d) => d.is_ccd) ? "CCD" : "Die";
});

// ---------- Derived ----------
const streaming = metricsStream.streaming;
const toggleStreamingBusy = metricsStream.toggleBusy;
const toggleStreaming = metricsStream.toggle;

const processCount = computed(() => processes.value.filter(p => !p.access_denied).length);

// Tree mode helpers
const processTree = computed(() => buildProcessTree(processes.value));
const searchWhitelist = computed(() => {
  if (!search.value) return null;
  return computeSearchWhitelist(processes.value, search.value);
});
const filteredTree = computed(() => {
  if (!searchWhitelist.value) return processTree.value;
  return filterTree(processTree.value, searchWhitelist.value.set);
});
const flatRows = computed(() => {
  if (searchWhitelist.value) {
    // In search mode, auto-expand matches
    const wl = searchWhitelist.value;
    if (wl) for (const pid of wl.needExpandPids) {
      if (!expandedPids.value.includes(pid)) expandedPids.value.push(pid);
    }
  }
  return flattenTree(filteredTree.value, new Set(expandedPids.value));
});
const tableRows = computed(() => {
  if (viewMode.value === "tree") return flatRows.value;
  // Flat mode: apply search filter
  if (!search.value) return processes.value.filter(p => !p.access_denied);
  const q = search.value.toLowerCase();
  return processes.value.filter(p =>
    !p.access_denied && (p.name.toLowerCase().includes(q) || String(p.pid) === search.value)
  );
});

// ---------- Functions ----------
function showSnack(text: string, color: "success" | "error" | "info" = "info") {
  snackbarText.value = text;
  snackbarColor.value = color;
  snackbar.value = true;
}

async function checkForUpdates() {
  if (updateLoading.value) return;
  updateLoading.value = true;
  try {
    const update: Update | null = await check();
    if (!update) {
      showSnack(t("noUpdateAvailable"), "info");
      return;
    }
    showSnack(t("updateInstalling", { version: update.version }), "info");
    await update.downloadAndInstall();
  } catch (e) {
    showSnack(t("updateCheckFailed", { error: String(e) }), "error");
  } finally {
    updateLoading.value = false;
  }
}

function rebuildProcessesByPid() {
  // Force reactivity update (processesByPid is computed)
}

function openEditor(p: ProcessInfo) {
  editorTarget.value = { kind: "process", process: p };
  editorOpen.value = true;
}

function onApplied() {
  showSnack(t("rulesApplied"), "success");
}

/** Non-Normal priority tier coloring for the priority column (Realtime/High = orange-red; BelowNormal/Idle = blue-gray) */
function prioStyle(p: ProcessInfo) {
  const color = priorityClassColor(p.priority_class);
  return color ? { color, fontWeight: 600 } : undefined;
}

/** Priority column tooltip: shows all three priority classes in detail */
function priorityTooltip(p: ProcessInfo): string {
  return `${t("prioCpu")}: ${priorityClassLabel(p.priority_class)}\n${t("prioIo")}: ${ioPriorityLabel(p.io_priority)}\n${t("prioMem")}: ${memoryPriorityLabel(p.memory_priority)}`;
}

function onRulesApplied(count: number) {
  showSnack(t("rulesAppliedTo", { count }), "success");
  refreshFull();
}

function toggleCpuScaleMode() {
  cpuScaleMode.value = cpuScaleMode.value === "per-core" ? "overall" : "per-core";
  // Refresh all display caches
  const baseline = getCpuBaseline();
  for (const p of processes.value) {
    refreshDisplayCache(p, baseline, { cpu: true }, cpuScaleMode.value, nproc.value);
  }
}

function updateTableHeight() {
  const el = tableCardRef.value;
  if (!el) return;
  const top = el.getBoundingClientRect().top;
  tableHeight.value = Math.max(280, window.innerHeight - top - 16);
}

// Context menu
const ctxMenuStyle = computed(() => {
  const { x, y } = ctxMenu.value;
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  return { left: Math.min(x, vw - 240) + "px", top: Math.min(y, vh - 200) + "px" };
});

function openProcessContextMenu(e: MouseEvent, p: ProcessInfo) {
  e.preventDefault();
  e.stopPropagation();
  ctxMenu.value = { open: true, x: e.clientX, y: e.clientY, type: "processRow", process: p };
}
function openCpuInfoContextMenu(e: MouseEvent) {
  e.preventDefault();
  e.stopPropagation();
  ctxMenu.value = { open: true, x: e.clientX, y: e.clientY, type: "cpuInfo", process: null };
}
function closeContextMenu() { ctxMenu.value.open = false; }

function preventDefaultContextMenu(e: MouseEvent) {
  e.preventDefault();
  if (ctxMenu.value.open) {
    const target = e.target as HTMLElement | null;
    if (!target || !target.closest(".ctx-menu")) ctxMenu.value.open = false;
  }
}
function onOutsideMouseDown(e: MouseEvent) {
  if (!ctxMenu.value.open) return;
  if (e.button === 2) return;
  const target = e.target as HTMLElement | null;
  if (target && target.closest(".ctx-menu")) return;
  ctxMenu.value.open = false;
}
function onScrollClose() { if (ctxMenu.value.open) ctxMenu.value.open = false; }

function ctxEdit() { const p = ctxMenu.value.process; if (p) openEditor(p); closeContextMenu(); }
function ctxReset() { const p = ctxMenu.value.process; if (p) resetAffinity(p); closeContextMenu(); }

/** Process path: prefer the enumeration result, otherwise query the backend on demand and backfill the row */
async function resolveProcessPath(p: ProcessInfo): Promise<string | null> {
  if (p.exe_path) return p.exe_path;
  const path = await getProcessExePath(p.pid).catch(() => null);
  if (path) p.exe_path = path;
  return path;
}

function ctxCopy(field: "pid" | "name" | "mask" | "path" | "all") {
  const p = ctxMenu.value.process;
  if (!p) return;
  if (field === "path") {
    resolveProcessPath(p).then((path) => {
      if (path) copyText(path);
      else showSnack(t("pathUnavailable"), "error");
    });
    closeContextMenu();
    return;
  }
  let text = "";
  if (field === "pid") text = String(p.pid);
  else if (field === "name") text = p.name;
  else if (field === "mask") text = p.affinity_mask ?? "";
  else text = `PID: ${p.pid}\n${t("name")}: ${p.name}\n${t("path")}: ${p.exe_path ?? t("unavailable")}\n${t("affinity")}: ${p.affinity_mask ?? "-"}\n${t("priority")}: ${priorityClassLabel(p.priority_class)}\n${t("cpu")}: ${p._display.cpu_text}\n${t("memory")}: ${formatMemory(p.memory_bytes)}`;
  copyText(text);
  closeContextMenu();
}
function ctxCopyCpuInfo() {
  if (!topology.value) return;
  const topo = topology.value;
  copyText(`${t("logicalProcessors")}: ${topo.total_logical_processors}\n${t("cores")}: ${topo.cores.length}\n${t("dies")}: ${topo.dies.length}`);
  closeContextMenu();
}
function ctxRefreshTopology() { initTopology(); closeContextMenu(); }

function copyText(text: string) {
  navigator.clipboard?.writeText(text).then(
    () => showSnack(t("copied"), "success"),
    () => showSnack(t("copyFailed"), "error"),
  );
}

function rowProps(ctx: { item: ProcessInfo }) {
  return {
    onContextmenu: (e: MouseEvent) => openProcessContextMenu(e, ctx.item),
    onClick: () => {
      if (viewMode.value !== "tree") return;
      const row = ctx.item as TableRow;
      if (row._hasChildren) toggleTreeNodeExpand(row.pid, ctx.item, expandedPids.value);
    },
  };
}

// ---------- Lifecycle ----------
onMounted(() => {
  document.addEventListener("contextmenu", preventDefaultContextMenu);
  document.addEventListener("mousedown", onOutsideMouseDown);
  window.addEventListener("scroll", onScrollClose, true);
  window.addEventListener("resize", updateTableHeight);
  nextTick(() => updateTableHeight());
  getVersion().then((version) => { appVersion.value = version; }).catch(() => {});

  // Register metrics listeners and start streaming
  metricsStream.register();
  metricsStream.start();
  refreshLogicalProcessorUsage();
  coreUsageTimer = setInterval(refreshLogicalProcessorUsage, 1000);

  // Bootstrap topology + processes in parallel
  initTopology();
  bootstrap().then(() => {
    nextTick(() => updateTableHeight());
    // Auto-apply affinity rules after bootstrap
    setTimeout(() => applyRules(), 2000);
  });
});

// Pre-load service status in background
refreshServiceStatus().catch(() => {});


onUnmounted(() => {
  document.removeEventListener("contextmenu", preventDefaultContextMenu);
  document.removeEventListener("mousedown", onOutsideMouseDown);
  window.removeEventListener("scroll", onScrollClose, true);
  window.removeEventListener("resize", updateTableHeight);
  metricsStream.unregister();
  if (coreUsageTimer) clearInterval(coreUsageTimer);
  coresResizeObserver?.disconnect();
  coresResizeObserver = null;
});
// ---------- Service Management ----------
const serviceStatusText = computed(() => {
  switch (serviceStatus.value) {
    case "running": return t("serviceRunning");
    case "stopped": return t("serviceStopped");
    case "not_installed": return t("serviceMissing");
    default: return t("serviceStatusUnknown", { status: serviceStatus.value });
  }
});
async function refreshServiceStatus() {
  serviceLoading.value = true;
  try {
    serviceStatus.value = await getServiceStatus();
  } catch (e: any) {
    serviceMessage.value = t("queryFailed", { error: String(e) });
  } finally {
    serviceLoading.value = false;
  }
}

async function openServiceDialog() {
  serviceDialogOpen.value = true;
  serviceMessage.value = "";
  await refreshServiceStatus();
}

async function doInstallService() {
  serviceLoading.value = true;
  serviceMessage.value = "";
  try {
    const msg = await installService();
    serviceMessage.value = msg;
    await refreshServiceStatus();
    showSnack(msg, "success");
  } catch (e: any) {
    serviceMessage.value = `${e}`;
    showSnack(t("installFailed", { error: String(e) }), "error");
  } finally {
    serviceLoading.value = false;
  }
}

async function doUninstallService() {
  serviceLoading.value = true;
  serviceMessage.value = "";
  try {
    const msg = await uninstallService();
    serviceMessage.value = msg;
    await refreshServiceStatus();
    showSnack(msg, "success");
  } catch (e: any) {
    serviceMessage.value = `${e}`;
    showSnack(t("uninstallFailed", { error: String(e) }), "error");
  } finally {
    serviceLoading.value = false;
  }
}

async function doStartService() {
  serviceLoading.value = true;
  try {
    const msg = await startService();
    await refreshServiceStatus();
    showSnack(msg, "success");
  } catch (e: any) {
    showSnack(t("startFailed", { error: String(e) }), "error");
  } finally {
    serviceLoading.value = false;
  }
}

async function doStopService() {
  serviceLoading.value = true;
  try {
    const msg = await stopService();
    await refreshServiceStatus();
    showSnack(msg, "success");
  } catch (e: any) {
    showSnack(t("stopFailed", { error: String(e) }), "error");
  } finally {
    serviceLoading.value = false;
  }
}
</script>

<template>
  <v-app>
    <!-- App Bar -->
    <v-app-bar flat color="surface" elevation="1">
      <v-icon icon="mdi-cpu-64-bit" class="ml-4 mr-2" color="primary" />
      <v-app-bar-title class="text-h6">
        {{ t('appTitle') }}
      </v-app-bar-title>
      <!-- CPU Info Bar -->
      <div class="cpu-info-bar d-none d-md-flex align-center mr-4" @contextmenu.prevent="openCpuInfoContextMenu">
        <v-chip size="small" variant="tonal" color="primary" class="mr-2">
          <v-icon icon="mdi-memory" start />
          {{ topology?.total_logical_processors ?? "-" }} LP
        </v-chip>
        <v-chip v-if="topology" size="small" variant="outlined" :color="isMultiDie ? 'success' : undefined" class="mr-2">
          {{ topology.cores.length }} {{ t('cores') }} / {{ topology.dies.length }}
          <span v-if="topology.dies.length > 0" class="ml-2 text-caption font-weight-bold text-uppercase" :class="isMultiDie ? 'text-success' : ''">
            {{ dieGroupLabel }}
          </span>
        </v-chip>
      </div>

      <v-spacer />
      <v-menu location="bottom end">
        <template #activator="{ props }">
          <v-btn v-bind="props" size="small" variant="text" prepend-icon="mdi-information-outline">
            {{ t('appVersion', { version: appVersionLabel }) }}
          </v-btn>
        </template>
        <v-list density="compact" min-width="210">
          <v-list-item :title="t('appVersion', { version: appVersionLabel })" prepend-icon="mdi-cpu-64-bit" />
          <v-divider />
          <v-list-item :title="t('checkForUpdates')" prepend-icon="mdi-update"
            :disabled="updateLoading" @click="checkForUpdates" />
        </v-list>
      </v-menu>

      <v-btn size="small" variant="text" prepend-icon="mdi-translate" @click="toggleLocale">{{ t('language') }}</v-btn>

      <!-- Theme toggle -->
      <v-tooltip :text="themeTooltip" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" :icon="themeIcon" variant="text" class="mr-2" @click="toggleTheme" />
        </template>
      </v-tooltip>

      <!-- CPU Scale Toggle -->
      <v-tooltip :text="cpuScaleTooltip" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" :icon="cpuScaleIcon" variant="text"
            :color="cpuScaleMode === 'overall' ? 'primary' : undefined" @click="toggleCpuScaleMode" />
        </template>
      </v-tooltip>

      <!-- Affinity rules -->
      <v-tooltip :text="t('rules')" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-bullseye" variant="text" color="info" @click="ruleManagerOpen = true" />
        </template>
      </v-tooltip>

      <!-- Dynamic optimization (ProBalance) -->
      <v-tooltip :text="t('pb')" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-tune-vertical" variant="text" color="purple-accent-2" @click="pbPanelOpen = true" />
        </template>
      </v-tooltip>

      <!-- Service Management -->
      <v-tooltip :text="t('service')" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-server-network" variant="text"
            :color="serviceStatus === 'running' ? 'success' : serviceStatus === 'not_installed' ? undefined : 'warning'"
            @click="openServiceDialog" />
        </template>
      </v-tooltip>

    </v-app-bar>

    <v-main>
      <v-container fluid class="pa-4">
        <!-- Toolbar -->
        <v-row dense class="mb-2 align-center">
          <v-col cols="12" sm="6" md="4">
            <v-text-field v-model="search" prepend-inner-icon="mdi-magnify"
              :placeholder="t('search')" density="compact" variant="outlined" hide-details clearable />
          </v-col>
          <v-col cols="auto">
            <span class="text-body-2 text-medium-emphasis">{{ t('processes') }} <strong>{{ processCount }}</strong></span>
          </v-col>
          <v-spacer />
          <v-col cols="auto">
            <!-- View mode toggle -->
            <v-tooltip :text="t('switchToFlat')" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="viewMode === 'flat' ? 'primary' : undefined"
                  prepend-icon="mdi-format-list-bulleted-square" @click="viewMode = 'flat'" class="mr-1">
                  {{ t('flat') }}
                </v-btn>
              </template>
            </v-tooltip>
            <v-tooltip :text="t('switchToTree')" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="viewMode === 'tree' ? 'primary' : undefined"
                  prepend-icon="mdi-family-tree" @click="viewMode = 'tree'" class="mr-2">
                  {{ t('tree') }}
                </v-btn>
              </template>
            </v-tooltip>
            <!-- Pause/Resume -->
            <v-tooltip :text="streaming ? t('pauseMetrics') : t('resumeMetrics')" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="streaming ? 'primary' : 'warning'"
                  :prepend-icon="streaming ? 'mdi-pause' : 'mdi-play'"
                  :disabled="toggleStreamingBusy" @click="toggleStreaming">
                  {{ streaming ? t('pause') : t('resume') }}
                </v-btn>
              </template>
            </v-tooltip>
          </v-col>
        </v-row>

        <!-- Loading bar -->
        <v-progress-linear v-if="loading && !processes.length" indeterminate color="primary" class="mb-2" />

        <div ref="cpuCoresCardRef">
          <v-card v-if="logicalProcessorUsage.length" variant="outlined" class="mb-2 pa-2">
            <div class="text-caption text-medium-emphasis mb-1">{{ t('cpu') }}</div>
            <div v-for="(row, ri) in coreRows" :key="ri" class="d-flex ga-2 mb-1">
              <div v-for="usage in row" :key="usage.index" class="core-usage">
                <span>{{ usage.index }}</span>
                <v-progress-linear :model-value="usage.usage_percent" height="5" rounded color="primary" />
              </div>
            </div>
          </v-card>
        </div>

        <!-- Process Table -->
        <div ref="tableCardRef">
          <v-card variant="outlined">
            <v-data-table :items="tableRows" :headers="[
              { title: 'PID', key: 'pid', sortable: true, width: '70px', minWidth: '70px' },
              { title: viewMode === 'tree' ? t('nameTree') : t('name'), key: 'name', sortable: true, width: '200px', minWidth: '150px' },
              { title: t('cpu'), key: 'cpu_usage_percent', sortable: true, width: '80px', minWidth: '80px', align: 'end' },
              { title: t('memory'), key: 'memory_bytes', sortable: true, width: '100px', minWidth: '100px', align: 'end' },
              { title: t('priority'), key: 'priority_class', sortable: true, width: '110px', minWidth: '110px' },
              { title: t('affinity'), key: 'affinity', sortable: false, width: '150px', minWidth: '140px' },
            ]"  :height="tableHeight" fixed-header density="compact" hover
              :header-props="{ class: 'font-weight-bold' }" item-value="pid"
              :row-props="rowProps" v-model:sort-by="sortBy"
              items-per-page="-1" hide-default-footer>

              <!-- PID -->
              <template #item.pid="{ item }">
                <code class="text-body-2">{{ item.pid }}</code>
              </template>

              <!-- Process Name -->
              <template #item.name="{ item }">
                <div class="d-flex align-center" :style="viewMode === 'tree' ? `padding-left: ${(item as any)._depth * 20}px` : ''">
                  <template v-if="viewMode === 'tree'">
                    <v-icon v-if="(item as any)._hasChildren" size="small" class="mr-1"
                      :icon="expandedPids.includes(item.pid) ? 'mdi-chevron-down' : 'mdi-chevron-right'"
                      @click.stop="toggleTreeNodeExpand(item.pid, item, expandedPids)" />
                    <v-icon v-else size="small" class="mr-1" icon="mdi-minus" color="grey-lighten-1" />
                  </template>
                  <v-icon size="small" class="mr-2" :icon="item.name.endsWith('.exe') ? 'mdi-application' : 'mdi-cog'" color="grey" />
                  <v-tooltip :text="item.exe_path ?? ''" location="top" :disabled="!item.exe_path">
                    <template #activator="{ props }">
                      <span class="text-body-2" v-bind="props">{{ item.name }}</span>
                    </template>
                  </v-tooltip>
                  <v-chip v-if="viewMode === 'tree' && (item as any)._hasChildren" size="x-small" variant="tonal" class="ml-2">
                    {{ countChildren(processes, item.pid) }}
                  </v-chip>
                </div>
              </template>

              <!-- CPU -->
              <template #item.cpu_usage_percent="{ item }">
                <span class="text-body-2 font-weight-medium" :style="{ color: item._display.cpu_color }">
                  {{ item._display.cpu_text }}
                </span>
              </template>

              <!-- Memory -->
              <template #item.memory_bytes="{ item }">
                <span v-if="item.memory_bytes > 0" class="text-body-2">{{ item._display.mem_text }}</span>
                <span v-else class="text-medium-emphasis">-</span>
              </template>

              <!-- Priority -->
              <template #item.priority_class="{ item }">
                <span class="text-body-2" :style="prioStyle(item)" :title="priorityTooltip(item)">
                  {{ priorityClassLabel(item.priority_class) }}
                </span>
              </template>

              <!-- Affinity -->
              <template #item.affinity="{ item }">
                <div v-if="item.access_denied" class="text-medium-emphasis text-body-2">
                  <v-icon icon="mdi-lock" size="small" class="mr-1" />{{ t('accessDenied') }}
                </div>
                <div v-else-if="!item.affinity_mask" class="text-medium-emphasis text-body-2">-</div>
                <div v-else class="d-flex align-center">
                  <div class="d-flex ga-1 flex-wrap">
                    <div v-for="bar in item._display.ccd_bars" :key="bar.id" class="affinity-bar"
                      :class="{ 'affinity-bar--empty': bar.enabled === 0 }"
                      role="img"
                      :aria-label="t('ccdBarTitle', { id: bar.id, enabled: bar.enabled, total: bar.total })"
                      :title="t('ccdBarTitle', { id: bar.id, enabled: bar.enabled, total: bar.total })"
                      :style="{ background: bar.enabled > 0 ? bar.color : 'transparent', borderColor: bar.enabled > 0 ? bar.color : undefined }">
                      {{ bar.enabled }}
                    </div>
                  </div>
                </div>
              </template>

              <!-- Empty State -->
              <template #no-data>
                <div class="text-center pa-6 text-medium-emphasis">
                  <v-icon icon="mdi-database-off-outline" size="large" class="mb-2" />
                  <div>{{ t('noData') }}</div>
                </div>
              </template>
            </v-data-table>
          </v-card>
        </div>
      </v-container>
    </v-main>

    <!-- Affinity Editor Dialog -->
    <AffinityEditor v-model="editorOpen" :target="editorTarget" :topology="topology" @applied="onApplied" />

    <!-- Affinity Rule Manager Dialog -->
    <AffinityRuleManager v-model="ruleManagerOpen" :topology="topology" @applied="onRulesApplied" />

    <!-- ProBalance Dynamic Optimization Panel -->
    <ProBalancePanel v-model="pbPanelOpen" />

    <!-- Service Management Dialog -->
    <v-dialog v-model="serviceDialogOpen" max-width="520" scroll-strategy="block">
      <v-card>
        <v-card-title class="d-flex align-center pa-3">
          <v-icon icon="mdi-server-network" class="mr-2" :color="serviceStatus === 'running' ? 'success' : 'warning'" />
          <span class="text-h6">{{ t('service') }}</span>
          <v-spacer />
          <v-btn icon="mdi-close" variant="text" density="compact" @click="serviceDialogOpen = false" />
        </v-card-title>
        <v-divider />
        <v-card-text class="pa-4">
          <v-alert
            :type="serviceStatus === 'running' ? 'success' : serviceStatus === 'not_installed' ? 'info' : 'warning'"
            density="compact" variant="tonal" class="mb-4"
            :text="serviceStatusText" />

          <p class="text-body-2 text-medium-emphasis mb-4">
            {{ t('serviceDesc') }}
          </p>

          <v-alert v-if="serviceMessage" type="info" density="compact" variant="outlined" class="mb-3" closable
            @click:close="serviceMessage = ''">
            {{ serviceMessage }}
          </v-alert>
        </v-card-text>
        <v-divider />
        <v-card-actions class="pa-3">
          <v-btn v-if="serviceStatus === 'not_installed'"
            color="primary" variant="flat" prepend-icon="mdi-download"
            :loading="serviceLoading" @click="doInstallService">
            {{ t('installAndStart') }}
          </v-btn>
          <template v-else>
            <v-btn v-if="serviceStatus === 'running'"
              color="warning" variant="tonal" prepend-icon="mdi-stop"
              :loading="serviceLoading" @click="doStopService">
              {{ t('stop') }}
            </v-btn>
            <v-btn v-else
              color="success" variant="tonal" prepend-icon="mdi-play"
              :loading="serviceLoading" @click="doStartService">
              {{ t('start') }}
            </v-btn>
            <v-btn color="error" variant="tonal" prepend-icon="mdi-delete"
              :loading="serviceLoading" @click="doUninstallService">
              {{ t('uninstall') }}
            </v-btn>
          </template>
          <v-spacer />
          <v-btn variant="text" prepend-icon="mdi-refresh" :loading="serviceLoading" @click="refreshServiceStatus">
            {{ t('refreshStatus') }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>

    <!-- Snackbar -->
    <v-snackbar v-model="snackbar" :color="snackbarColor" timeout="3000">
      {{ snackbarText }}
      <template #actions>
        <v-btn icon="mdi-close" variant="text" @click="snackbar = false" />
      </template>
    </v-snackbar>

    <!-- Context Menu -->
    <Teleport to="body">
      <div v-if="ctxMenu.open" class="ctx-menu elevation-8" :style="ctxMenuStyle"
        @contextmenu.prevent.stop @click.stop>
        <v-list density="compact" nav min-width="220">
          <!-- Process Row Menu -->
          <template v-if="ctxMenu.type === 'processRow' && ctxMenu.process">
            <v-list-item prepend-icon="mdi-pencil" base-color="primary"
              :disabled="ctxMenu.process.access_denied || !ctxMenu.process.affinity_mask" @click="ctxEdit">
              <v-list-item-title>{{ t('editRules') }}</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-undo-variant"
              :disabled="ctxMenu.process.access_denied || !ctxMenu.process.system_affinity_mask" @click="ctxReset">
              <v-list-item-title>{{ t('resetDefault') }}</v-list-item-title>
            </v-list-item>
            <v-divider class="my-1" />
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopy('pid')">
              <v-list-item-title>{{ t('copyPid') }}</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopy('name')">
              <v-list-item-title>{{ t('copyName') }}</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopy('path')">
              <v-list-item-title>{{ t('copyPath') }}</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-content-copy" :disabled="!ctxMenu.process.affinity_mask" @click="ctxCopy('mask')">
              <v-list-item-title>{{ t('copyMask') }}</v-list-item-title>
            </v-list-item>
            <v-divider class="my-1" />
            <v-list-item prepend-icon="mdi-clipboard-text-outline" @click="ctxCopy('all')">
              <v-list-item-title>{{ t('copyAll') }}</v-list-item-title>
            </v-list-item>
          </template>

          <!-- CPU Info Menu -->
          <template v-else-if="ctxMenu.type === 'cpuInfo'">
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopyCpuInfo">
              <v-list-item-title>{{ t('copyCpu') }}</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-refresh" @click="ctxRefreshTopology">
              <v-list-item-title>{{ t('refreshTopology') }}</v-list-item-title>
            </v-list-item>
          </template>
        </v-list>
      </div>
    </Teleport>
  </v-app>
</template>

<style>
/* Prevent page scroll - constrain everything to viewport */
html, body {
  overflow: hidden !important;
  height: 100vh !important;
}
.v-application {
  height: 100vh !important;
  overflow: hidden !important;
}
:deep(.v-main) {
  height: calc(100vh - 64px) !important;
  overflow: hidden !important;
}
:deep(.v-container) {
  height: 100% !important;
  overflow: hidden !important;
}
/* Force fixed column widths - prevent resize on data change */
:deep(.v-data-table table) {
  table-layout: fixed !important;
  width: 100% !important;
}
:deep(.v-data-table th),
:deep(.v-data-table td) {
  overflow: hidden !important;
  text-overflow: ellipsis !important;
  white-space: nowrap !important;
}

.core-usage {
  width: 54px;
  font-size: 11px;
}
.ctx-menu {
  position: fixed;
  border-radius: 6px;
  overflow: hidden;
  animation: ctx-menu-in 80ms ease-out;
  transform-origin: top left;
  z-index: 9999;
}
@keyframes ctx-menu-in {
  from { opacity: 0; transform: scale(0.96); }
  to { opacity: 1; transform: scale(1); }
}

/* ---------- Dark theme: soften the pure-white outlined-component border ----------
 * Vuetify's outlined components default to `border: currentColor`, which in dark
 * mode resolves to pure white (on-surface) and is visually too harsh. Swap to
 * a neutral gray, while keeping Vuetify's own hover/focus/error state logic. */
.v-theme--dark {
  --cpum-outline: rgba(148, 148, 148, 0.38);
}
.v-theme--dark .v-btn--variant-outlined,
.v-theme--dark .v-chip--variant-outlined,
.v-theme--dark .v-card--variant-outlined,
.v-theme--dark .v-alert--variant-outlined {
  border-color: var(--cpum-outline);
}
/* Input field outline: use a solid neutral gray, and let --v-field-border-opacity
 * own the transparency (hover/focus brighten it). Excludes the error state so
 * the red error border is preserved. */
.v-theme--dark .v-field--variant-outlined:not(.v-field--error) .v-field__outline__start,
.v-theme--dark .v-field--variant-outlined:not(.v-field--error) .v-field__outline__notch::before,
.v-theme--dark .v-field--variant-outlined:not(.v-field--error) .v-field__outline__notch::after,
.v-theme--dark .v-field--variant-outlined:not(.v-field--error) .v-field__outline__end {
  border-color: rgb(148, 148, 148);
}
</style>

<style scoped>
.affinity-bar {
  width: 20px;
  height: 20px;
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1px solid;
  border-radius: 4px;
  font-size: 11px;
  font-weight: 600;
  color: #fff;
  text-shadow: 0 1px 2px rgba(0, 0, 0, 0.5);
  cursor: help;
  transition: background 0.15s ease, border-color 0.15s ease, color 0.15s ease;
}
.affinity-bar--empty {
  border-color: rgba(128, 128, 128, 0.28);
  color: rgba(128, 128, 128, 0.75);
  text-shadow: none;
}
.cpu-info-bar :deep(.v-chip) {
  font-weight: 500;
}
</style>



