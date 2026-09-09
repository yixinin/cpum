<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, nextTick } from "vue";
import type { ProcessInfo, CpuScaleMode } from "./types";
import { refreshDisplayCache, formatMemory } from "./types";
import { getServiceStatus, installService, uninstallService, startService, stopService, type ServiceStatus } from "./api";
import type { SortItem, ViewMode } from "./constants";
import { useTopology } from "./composables/useTopology";
import { useProcessManager } from "./composables/useProcessManager";
import { useMetricsStream } from "./composables/useMetricsStream";
import { buildProcessTree, flattenTree, computeSearchWhitelist, filterTree, countChildren, toggleTreeNodeExpand, type TableRow } from "./composables/useProcessTree";
import AffinityEditor from "./components/AffinityEditor.vue";
import AffinityRuleManager from "./components/AffinityRuleManager.vue";

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

// Affinity editor
const editorOpen = ref(false);
const editingProcess = ref<ProcessInfo | null>(null);

// Rule manager
const ruleManagerOpen = ref(false);
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
const cpuScaleTooltip = computed(() => cpuScaleMode.value === "overall" ? "显示: 整体 CPU (类伻任务管理器)" : "显示: 单核基准 (每核 100%)");

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

function rebuildProcessesByPid() {
  // Force reactivity update (processesByPid is computed)
}

function openEditor(p: ProcessInfo) {
  editingProcess.value = p;
  editorOpen.value = true;
}

function onApplied() {
  showSnack("亲和性规则已应用", "success");
}

function onRulesApplied(count: number) {
  showSnack(`Rules applied to ${count}  processes`, "success");
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
function ctxCopy(field: "pid" | "name" | "mask" | "all") {
  const p = ctxMenu.value.process;
  if (!p) return;
  let text = "";
  if (field === "pid") text = String(p.pid);
  else if (field === "name") text = p.name;
  else if (field === "mask") text = p.affinity_mask ?? "";
  else text = `PID: ${p.pid}\nName: ${p.name}\nAffinity: ${p.affinity_mask ?? "-"}\nCPU: ${p._display.cpu_text}\nMem: ${formatMemory(p.memory_bytes)}`;
  copyText(text);
  closeContextMenu();
}
function ctxCopyCpuInfo() {
  if (!topology.value) return;
  const t = topology.value;
  copyText(`Logical Processors: ${t.total_logical_processors}\nCores: ${t.cores.length}\nDies/CCDs: ${t.dies.length}`);
  closeContextMenu();
}
function ctxRefreshTopology() { initTopology(); closeContextMenu(); }

function copyText(text: string) {
  navigator.clipboard?.writeText(text).then(
    () => showSnack("Copied to clipboard", "success"),
    () => showSnack("Copy failed", "error"),
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

  // Register metrics listeners and start streaming
  metricsStream.register();
  metricsStream.start();

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
});
// ---------- Service Management ----------
const serviceStatusText = computed(() => {
  switch (serviceStatus.value) {
    case "running": return "服务运行中。开机时自动应用规则。";
    case "stopped": return "服务已安装但已停止。";
    case "not_installed": return "服务未安装。安装后可开机自动应用规则。";
    default: return `Service status: ${serviceStatus.value}`;
  }
});
async function refreshServiceStatus() {
  serviceLoading.value = true;
  try {
    serviceStatus.value = await getServiceStatus();
  } catch (e: any) {
    serviceMessage.value = `Query failed: ${e}`;
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
    showSnack(`Install failed: ${e}`, "error");
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
    showSnack(`Uninstall failed: ${e}`, "error");
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
    showSnack(`Start failed: ${e}`, "error");
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
    showSnack(`Stop failed: ${e}`, "error");
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
        CPU Manager
      </v-app-bar-title>
      <v-spacer />

      <!-- CPU Info Bar -->
      <div class="cpu-info-bar d-none d-md-flex align-center mr-4" @contextmenu.prevent="openCpuInfoContextMenu">
        <v-chip size="small" variant="tonal" color="primary" class="mr-2">
          <v-icon icon="mdi-memory" start />
          {{ topology?.total_logical_processors ?? "-" }} LP
        </v-chip>
        <v-chip v-if="topology" size="small" variant="outlined" class="mr-2">
          {{ topology.cores.length }} cores / {{ topology.dies.length }}
          <v-chip v-if="topology.dies.length > 1" size="x-small" color="success" class="ml-1">CCD</v-chip>
          <v-chip v-else size="small" variant="tonal">
            <v-icon icon="mdi-view-module" start />
            CCD
          </v-chip>
        </v-chip>
      </div>

      <!-- CPU Scale Toggle -->
      <v-tooltip :text="cpuScaleTooltip" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" :icon="cpuScaleIcon" variant="text"
            :color="cpuScaleMode === 'overall' ? 'primary' : undefined" @click="toggleCpuScaleMode" />
        </template>
      </v-tooltip>

      <!-- 亲和性规则 -->
      <v-tooltip text="亲和性规则" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-ruler" variant="text" color="info" @click="ruleManagerOpen = true" />
        </template>
      </v-tooltip>

      <!-- Service Management -->
      <v-tooltip text="开机自启服务" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-server-network" variant="text"
            :color="serviceStatus === 'running' ? 'success' : serviceStatus === 'not_installed' ? undefined : 'warning'"
            @click="openServiceDialog" />
        </template>
      </v-tooltip>

      <!-- Refresh -->
      <v-tooltip text="Refresh (Full Reload)" location="bottom">
        <template #activator="{ props }">
          <v-btn v-bind="props" icon="mdi-refresh" variant="text" :loading="loading" @click="refreshFull" />
        </template>
      </v-tooltip>
    </v-app-bar>

    <v-main>
      <v-container fluid class="pa-4">
        <!-- Toolbar -->
        <v-row dense class="mb-2 align-center">
          <v-col cols="12" sm="6" md="4">
            <v-text-field v-model="search" prepend-inner-icon="mdi-magnify"
              placeholder="Search process name or PID..." density="compact" variant="outlined" hide-details clearable />
          </v-col>
          <v-col cols="auto">
            <span class="text-body-2 text-medium-emphasis">Processes <strong>{{ processCount }}</strong></span>
          </v-col>
          <v-spacer />
          <v-col cols="auto">
            <!-- View mode toggle -->
            <v-tooltip text="切换: 平坦列表" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="viewMode === 'flat' ? 'primary' : undefined"
                  prepend-icon="mdi-format-list-bulleted-square" @click="viewMode = 'flat'" class="mr-1">
                  Flat
                </v-btn>
              </template>
            </v-tooltip>
            <v-tooltip text="切换: 进程树" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="viewMode === 'tree' ? 'primary' : undefined"
                  prepend-icon="mdi-family-tree" @click="viewMode = 'tree'" class="mr-2">
                  Tree
                </v-btn>
              </template>
            </v-tooltip>
            <!-- Pause/Resume -->
            <v-tooltip :text="streaming ? '暂停指标' : '继续指标'" location="bottom">
              <template #activator="{ props }">
                <v-btn v-bind="props" size="small" variant="tonal"
                  :color="streaming ? 'primary' : 'warning'"
                  :prepend-icon="streaming ? 'mdi-pause' : 'mdi-play'"
                  :disabled="toggleStreamingBusy" @click="toggleStreaming">
                  {{ streaming ? "Pause" : "Resume" }}
                </v-btn>
              </template>
            </v-tooltip>
          </v-col>
        </v-row>

        <v-alert v-if="false" type="error" density="compact" class="mb-3" closable />

        <!-- Loading bar -->
        <v-progress-linear v-if="loading && !processes.length" indeterminate color="primary" class="mb-2" />

        <!-- Process Table -->
        <div ref="tableCardRef">
          <v-card variant="outlined">
            <v-data-table :items="tableRows" :headers="[
              { title: 'PID', key: 'pid', sortable: true, width: '70px', minWidth: '70px' },
              { title: viewMode === 'tree' ? 'Name (Tree)' : 'Name', key: 'name', sortable: true, width: '200px', minWidth: '150px' },
              { title: 'CPU', key: 'cpu_usage_percent', sortable: true, width: '80px', minWidth: '80px', align: 'end' },
              { title: 'Memory', key: 'memory_bytes', sortable: true, width: '100px', minWidth: '100px', align: 'end' },
              { title: 'Affinity', key: 'affinity', sortable: false, width: '100px', minWidth: '100px' },
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
                  <span class="text-body-2">{{ item.name }}</span>
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

              <!-- Affinity -->
              <template #item.affinity="{ item }">
                <div v-if="item.access_denied" class="text-medium-emphasis text-body-2">
                  <v-icon icon="mdi-lock" size="small" class="mr-1" />权限不足
                </div>
                <div v-else-if="!item.affinity_mask" class="text-medium-emphasis text-body-2">-</div>
                <div v-else class="d-flex align-center">
                  <div class="d-flex ga-1">
                    <div v-for="bar in item._display.ccd_bars" :key="bar.id" class="affinity-bar"
                      :title="`CCD ${bar.id}: Enabled: ${bar.enabled} / ${bar.total}  threads`"
                      :style="{ background: bar.enabled > 0 ? bar.color : 'transparent', borderColor: bar.color }">
                      {{ bar.enabled }}
                    </div>
                  </div>
                </div>
              </template>

              <!-- Empty State -->
              <template #no-data>
                <div class="text-center pa-6 text-medium-emphasis">
                  <v-icon icon="mdi-database-off-outline" size="large" class="mb-2" />
                  <div>无进程数据</div>
                </div>
              </template>
            </v-data-table>
          </v-card>
        </div>
      </v-container>
    </v-main>

    <!-- Affinity Editor Dialog -->
    <AffinityEditor v-model="editorOpen" :process="editingProcess" :topology="topology" @applied="onApplied" />

    <!-- Affinity Rule Manager Dialog -->
    <AffinityRuleManager v-model="ruleManagerOpen" :topology="topology" @applied="onRulesApplied" />

    <!-- Service Management Dialog -->
    <v-dialog v-model="serviceDialogOpen" max-width="520" scroll-strategy="block">
      <v-card>
        <v-card-title class="d-flex align-center pa-3">
          <v-icon icon="mdi-server-network" class="mr-2" :color="serviceStatus === 'running' ? 'success' : 'warning'" />
          <span class="text-h6">开机自启服务</span>
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
            安装后，CPU 亲和性规则会在开机时自动应用到匹配的进程。
            服务以 SYSTEM 身份运行，每 5 秒扫描一次。
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
            安装并启动
          </v-btn>
          <template v-else>
            <v-btn v-if="serviceStatus === 'running'"
              color="warning" variant="tonal" prepend-icon="mdi-stop"
              :loading="serviceLoading" @click="doStopService">
              Stop
            </v-btn>
            <v-btn v-else
              color="success" variant="tonal" prepend-icon="mdi-play"
              :loading="serviceLoading" @click="doStartService">
              Start
            </v-btn>
            <v-btn color="error" variant="tonal" prepend-icon="mdi-delete"
              :loading="serviceLoading" @click="doUninstallService">
              Uninstall
            </v-btn>
          </template>
          <v-spacer />
          <v-btn variant="text" prepend-icon="mdi-refresh" :loading="serviceLoading" @click="refreshServiceStatus">
            刷新状态
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
              <v-list-item-title>Edit Affinity</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-undo-variant"
              :disabled="ctxMenu.process.access_denied || !ctxMenu.process.system_affinity_mask" @click="ctxReset">
              <v-list-item-title>Reset to Default</v-list-item-title>
            </v-list-item>
            <v-divider class="my-1" />
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopy('pid')">
              <v-list-item-title>Copy PID</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopy('name')">
              <v-list-item-title>复制进程名</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-content-copy" :disabled="!ctxMenu.process.affinity_mask" @click="ctxCopy('mask')">
              <v-list-item-title>复制亲和性掩码</v-list-item-title>
            </v-list-item>
            <v-divider class="my-1" />
            <v-list-item prepend-icon="mdi-clipboard-text-outline" @click="ctxCopy('all')">
              <v-list-item-title>复制全部 Info</v-list-item-title>
            </v-list-item>
          </template>

          <!-- CPU Info Menu -->
          <template v-else-if="ctxMenu.type === 'cpuInfo'">
            <v-list-item prepend-icon="mdi-content-copy" @click="ctxCopyCpuInfo">
              <v-list-item-title>复制 CPU 信息</v-list-item-title>
            </v-list-item>
            <v-list-item prepend-icon="mdi-refresh" @click="ctxRefreshTopology">
              <v-list-item-title>Refresh Topology</v-list-item-title>
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
</style>

<style scoped>
.affinity-bar {
  width: 28px;
  height: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 1.5px solid;
  border-radius: 4px;
  font-size: 11px;
  font-weight: 600;
  color: #fff;
  text-shadow: 0 1px 2px rgba(0, 0, 0, 0.5);
  cursor: help;
}
.cpu-info-bar :deep(.v-chip) {
  font-weight: 500;
}
</style>



