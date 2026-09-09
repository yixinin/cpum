<script setup lang="ts">
import { ref, computed, watch } from "vue";
import type { CpuTopology, ProcessInfo, LogicalProcessorInfo } from "../types";
import { parseMask, formatMask, popcount, getBit, setBit } from "../types";
import { setProcessAffinity, loadAffinityRules, addAffinityRule, updateAffinityRule } from "../api";

const props = defineProps<{
  modelValue: boolean;
  process: ProcessInfo | null;
  topology: CpuTopology | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
  applied: [];
}>();

// 当前编辑的 mask (bigint)
const editingMask = ref<bigint>(0n);
// 原始 mask (用于 Reset)
const originalMask = ref<bigint>(0n);
// 系统 mask (限制可选项)
const systemMask = ref<bigint>(0n);
const applying = ref(false);
const errorMsg = ref<string | null>(null);

// 当 dialog 打开 / 进程变化时, 初始化
watch(
  () => [props.modelValue, props.process],
  () => {
    if (props.modelValue && props.process) {
      originalMask.value = parseMask(props.process.affinity_mask);
      editingMask.value = originalMask.value;
      systemMask.value = parseMask(props.process.system_affinity_mask);
      errorMsg.value = null;
    }
  },
  { immediate: true }
);

// LP index -> LogicalProcessorInfo 查找表
const lpMap = computed(() => {
  const m = new Map<number, LogicalProcessorInfo>();
  if (props.topology) {
    for (const lp of props.topology.logical_processors) m.set(lp.index, lp);
  }
  return m;
});

// 按 CCD 分组的逻辑处理器列表 (用于渲染)
const diesWithLps = computed(() => {
  if (!props.topology) return [];
  return props.topology.dies.map((die) => ({
    die,
    lps: die.threads
      .map((idx) => lpMap.value.get(idx))
      .filter((x): x is LogicalProcessorInfo => !!x)
      .sort((a, b) => a.index - b.index),
  }));
});

const selectedCount = computed(() => popcount(editingMask.value));
const totalCount = computed(() => props.topology?.total_logical_processors ?? 0);
const maskHex = computed(() => formatMask(editingMask.value));

// CCD 颜色调色板 (最多 8 个 CCD, 颜色高对比度)
const CCD_COLORS = [
  "#42A5F5", // 蓝
  "#66BB6A", // 绿
  "#FFA726", // 橙
  "#EF5350", // 红
  "#AB47BC", // 紫
  "#26C6DA", // 青
  "#FFEE58", // 黄
  "#8D6E63", // 棕
];

function ccdColor(dieId: number): string {
  return CCD_COLORS[dieId % CCD_COLORS.length];
}

function isAvailable(bit: number): boolean {
  return getBit(systemMask.value, bit);
}
function isActive(bit: number): boolean {
  return getBit(editingMask.value, bit);
}
function toggleBit(bit: number) {
  if (!isAvailable(bit)) return;
  editingMask.value = setBit(editingMask.value, bit, !isActive(bit));
}

// ---------- 快速选择 ----------

function selectAll() {
  editingMask.value = systemMask.value;
}
function selectNone() {
  editingMask.value = 0n;
}
function selectPrimary() {
  if (!props.topology) return;
  let m = 0n;
  for (const lp of props.topology.logical_processors) {
    if (lp.smt_thread_id === 0 && isAvailable(lp.index)) {
      m = setBit(m, lp.index, true);
    }
  }
  editingMask.value = m;
}
function selectSecondary() {
  if (!props.topology) return;
  let m = 0n;
  for (const lp of props.topology.logical_processors) {
    if (lp.smt_thread_id > 0 && isAvailable(lp.index)) {
      m = setBit(m, lp.index, true);
    }
  }
  editingMask.value = m;
}
function resetMask() {
  editingMask.value = originalMask.value;
}
function selectCcd(dieId: number) {
  if (!props.topology) return;
  const die = props.topology.dies.find((d) => d.id === dieId);
  if (!die) return;
  let m = editingMask.value;
  for (const bit of die.threads) {
    if (isAvailable(bit)) m = setBit(m, bit, true);
  }
  editingMask.value = m;
}
function clearCcd(dieId: number) {
  if (!props.topology) return;
  const die = props.topology.dies.find((d) => d.id === dieId);
  if (!die) return;
  let m = editingMask.value;
  for (const bit of die.threads) m = setBit(m, bit, false);
  editingMask.value = m;
}

async function apply() {
  if (!props.process) return;
  applying.value = true;
  errorMsg.value = null;
  try {
    await setProcessAffinity(props.process.pid, editingMask.value);
    originalMask.value = editingMask.value;
    
    // Persist the setting even when the mask itself was unchanged.
    const maskHex = formatMask(editingMask.value);
    const processName = props.process.name.replace(/\.exe$/i, "");
    const rules = await loadAffinityRules();
    const existing = rules.find((rule) =>
      rule.process_name.replace(/\.exe$/i, "").toLowerCase() === processName.toLowerCase()
    );
    if (existing) {
      await updateAffinityRule(existing.id, { mask: maskHex });
    } else {
      await addAffinityRule(processName, maskHex);
    }
    
    emit("applied");
    emit("update:modelValue", false);
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
  } finally {
    applying.value = false;
  }
}

function close() {
  emit("update:modelValue", false);
}
</script>

<template>
  <v-dialog
    :model-value="modelValue"
    max-width="920"
    persistent
    scroll-strategy="block"
    @update:model-value="emit('update:modelValue', $event)"
  >
    <v-card v-if="process">
      <v-card-title class="d-flex align-center pa-3">
        <v-icon icon="mdi-chip" class="mr-2" />
        <span class="text-h6">CPU 亲和性 - {{ process.name }}</span>
        <v-chip size="small" color="primary" variant="tonal" class="ml-2">PID {{ process.pid }}</v-chip>
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" density="compact" @click="close" />
      </v-card-title>

      <v-divider />

      <v-card-text class="pa-4">
        <!-- 快速选择工具栏 -->
        <div class="d-flex align-center flex-wrap mb-3">
          <span class="text-subtitle-2 mr-2">快速选择:</span>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectAll">全部</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectNone">清空</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectPrimary">仅主线程</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectSecondary">仅副线程</v-btn>
          <v-btn size="small" variant="outlined" class="mb-1" @click="resetMask">重置到原值</v-btn>
        </div>

        <v-alert v-if="errorMsg" type="error" density="compact" class="mb-3" closable @click:close="errorMsg = null">
          {{ errorMsg }}
        </v-alert>

        <v-alert
          v-if="topology && !topology.single_group"
          type="warning"
          density="compact"
          class="mb-3"
        >
          当前系统存在多个处理器组, 仅支持编辑默认组的亲和性。
        </v-alert>

        <!-- CCD 分组渲染 -->
        <div v-if="!topology" class="text-center text-medium-emphasis pa-4">
          正在加载 CPU 拓扑...
        </div>

        <div v-for="{ die, lps } in diesWithLps" :key="die.id" class="ccd-section mb-4">
          <div class="d-flex align-center mb-2">
            <span
              class="ccd-dot mr-2"
              :style="{ background: ccdColor(die.id) }"
            />
            <span class="text-subtitle-2">
              {{ die.is_ccd ? `CCD ${die.id}` : `逻辑 Die ${die.id}` }}
            </span>
            <span class="text-caption text-medium-emphasis ml-2">
              {{ die.cores.length }} 物理核 / {{ die.threads.length }} 逻辑线程
            </span>
            <v-spacer />
            <v-btn
              size="x-small"
              variant="tonal"
              class="mr-1"
              :style="{ color: ccdColor(die.id) }"
              @click="selectCcd(die.id)"
            >
              选此 CCD
            </v-btn>
            <v-btn size="x-small" variant="text" @click="clearCcd(die.id)">清除</v-btn>
          </div>

          <div class="cpu-grid">
            <button
              v-for="lp in lps"
              :key="lp.index"
              type="button"
              class="cpu-box"
              :class="{
                active: isActive(lp.index),
                'smt-secondary': lp.is_smt_secondary,
                'smt-primary': !lp.is_smt_secondary,
                unavailable: !isAvailable(lp.index),
              }"
              :style="{ '--ccd': ccdColor(die.id) }"
              :title="`LP ${lp.index} · Core ${lp.core_id}${lp.is_smt_secondary ? ' (SMT 副线程)' : ' (主线程)'}`"
              @click="toggleBit(lp.index)"
            >
              {{ lp.index }}
            </button>
          </div>
        </div>
      </v-card-text>

      <v-divider />

      <v-card-actions class="pa-3">
        <span class="text-body-2 ml-2">
          已选 <strong class="text-primary">{{ selectedCount }}</strong> / {{ totalCount }} 逻辑处理器
        </span>
        <span class="text-body-2 text-medium-emphasis ml-4">
          Mask: <code>{{ maskHex }}</code>
        </span>
        <v-spacer />
        <v-btn variant="text" :disabled="applying" @click="close">取消</v-btn>
        <v-btn
          color="primary"
          variant="flat"
          :loading="applying"
          @click="apply"
        >
          应用
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<style scoped>
.ccd-section {
  padding: 8px 12px;
  background: rgba(255, 255, 255, 0.02);
  border-radius: 6px;
  border: 1px solid rgba(255, 255, 255, 0.05);
}

.ccd-dot {
  display: inline-block;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  border: 1px solid rgba(255, 255, 255, 0.2);
}

.cpu-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(38px, 1fr));
  gap: 4px;
}

.cpu-box {
  width: 100%;
  height: 36px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 2px solid var(--ccd);
  border-radius: 5px;
  background: transparent;
  color: var(--ccd);
  font-size: 13px;
  font-weight: 600;
  cursor: pointer;
  user-select: none;
  transition: background 0.12s ease, color 0.12s ease, transform 0.08s ease;
  font-family: "Cascadia Code", "Consolas", monospace;
}

.cpu-box:hover:not(.unavailable) {
  background: color-mix(in srgb, var(--ccd) 22%, transparent);
}

.cpu-box:active:not(.unavailable) {
  transform: scale(0.92);
}

.cpu-box.active {
  background: var(--ccd);
  color: #fff;
}

.cpu-box.active:hover:not(.unavailable) {
  background: color-mix(in srgb, var(--ccd) 80%, white);
}

/* SMT 副线程: 虚线边框 */
.cpu-box.smt-secondary {
  border-style: dashed;
}
.cpu-box.smt-secondary.active {
  border-style: dashed;
  background: color-mix(in srgb, var(--ccd) 60%, transparent);
}

.cpu-box.unavailable {
  opacity: 0.25;
  cursor: not-allowed;
  border-color: #757575;
  color: #757575;
  background: transparent;
}
.cpu-box.unavailable:hover {
  background: transparent;
}
</style>
