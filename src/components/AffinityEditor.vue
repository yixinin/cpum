<script setup lang="ts">
import { ref, computed, watch } from "vue";
import type { CpuTopology, ProcessInfo, LogicalProcessorInfo, RuleMode } from "../types";
import { parseMask, formatMask, popcount, getBit, setBit, PRIORITY_CLASS_OPTIONS, IO_PRIORITY_OPTIONS, MEMORY_PRIORITY_OPTIONS } from "../types";
import type { AffinityRule } from "../api";
import { setProcessAffinity, setProcessPriority, loadAffinityRules, addAffinityRule, updateAffinityRule } from "../api";
import { useI18n } from "../i18n";
const { t } = useI18n();

const props = defineProps<{
  modelValue: boolean;
  process: ProcessInfo | null;
  topology: CpuTopology | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
  applied: [];
}>();

// The mask currently being edited (bigint)
const editingMask = ref<bigint>(0n);
// The original mask (used for Reset)
const originalMask = ref<bigint>(0n);
// The system mask (restricts which LPs are selectable)
const systemMask = ref<bigint>(0n);
const applying = ref(false);
const errorMsg = ref<string | null>(null);

// ---------- Priority edit state (M1) ----------
const editingPrioClass = ref<number | null>(null);
const editingIo = ref<number | null>(null);
const editingMem = ref<number | null>(null);
const originalPrioClass = ref<number | null>(null);
const originalIo = ref<number | null>(null);
const originalMem = ref<number | null>(null);

// ---------- Scheduling mode + save as rule (M1/M2) ----------
/** strict = hard mask pins cores; soft = elastic CPU Sets (auto-falls back to hard mask on older systems) */
const affinityMode = ref<RuleMode>("strict");
/** Only persisted when the user explicitly checks "Save as rule" — replaces the old silent-save behavior */
const saveAsRule = ref(false);

// Initialize when the dialog opens / the process changes
watch(
  () => [props.modelValue, props.process],
  () => {
    if (props.modelValue && props.process) {
      originalMask.value = props.process.group_affinity_masks
        ? props.process.group_affinity_masks.reduce((all, value, group) => all | (parseMask(value) << BigInt(group * 64)), 0n)
        : parseMask(props.process.affinity_mask);
      editingMask.value = originalMask.value;
      systemMask.value = props.process.group_system_affinity_masks
        ? props.process.group_system_affinity_masks.reduce((all, value, group) => all | (parseMask(value) << BigInt(group * 64)), 0n)
        : parseMask(props.process.system_affinity_mask);
      originalPrioClass.value = props.process.priority_class;
      editingPrioClass.value = props.process.priority_class;
      originalIo.value = props.process.io_priority;
      editingIo.value = props.process.io_priority;
      originalMem.value = props.process.memory_priority;
      editingMem.value = props.process.memory_priority;
      affinityMode.value = "strict";
      saveAsRule.value = false;
      errorMsg.value = null;
    }
  },
  { immediate: true }
);

// Dropdown options (computed to stay reactive to locale changes)
const prioClassItems = computed(() =>
  PRIORITY_CLASS_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
);
const ioPriorityItems = computed(() =>
  IO_PRIORITY_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
);
const memPriorityItems = computed(() =>
  MEMORY_PRIORITY_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
);

const priorityDirty = computed(
  () =>
    editingPrioClass.value !== originalPrioClass.value ||
    editingIo.value !== originalIo.value ||
    editingMem.value !== originalMem.value,
);

function resetPriorities() {
  editingPrioClass.value = originalPrioClass.value;
  editingIo.value = originalIo.value;
  editingMem.value = originalMem.value;
}

// LP index -> LogicalProcessorInfo lookup table
const lpMap = computed(() => {
  const m = new Map<number, LogicalProcessorInfo>();
  if (props.topology) {
    for (const lp of props.topology.logical_processors) m.set(lp.index, lp);
  }
  return m;
});

// LPs grouped by die/CCD (for rendering)
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
const groupMaskStrings = computed(() => {
  const count = props.topology?.group_count ?? 1;
  return Array.from({ length: count }, (_, group) =>
    formatMask((editingMask.value >> BigInt(group * 64)) & ((1n << 64n) - 1n)),
  );
});

// CCD color palette (up to 8 CCDs, high-contrast colors)
const CCD_COLORS = [
  "#42A5F5", // blue
  "#66BB6A", // green
  "#FFA726", // orange
  "#EF5350", // red
  "#AB47BC", // purple
  "#26C6DA", // cyan
  "#FFEE58", // yellow
  "#8D6E63", // brown
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

// ---------- Quick select ----------

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
  if (editingMask.value === 0n) {
    errorMsg.value = t("atLeastOneLp");
    return;
  }
  applying.value = true;
  errorMsg.value = null;
  // Snapshot priority-dirty first (apply() will write back into original*, after which dirty would be false)
  const prioDirty = priorityDirty.value;
  try {
    // 1. Apply immediately: write per the scheduling mode (soft = CPU Sets; auto-falls back to hard mask on Win10 pre-1803)
    await setProcessAffinity(props.process.pid, editingMask.value, affinityMode.value, groupMaskStrings.value);
    originalMask.value = editingMask.value;

    // 2. Priorities: only write the fields the user actually changed
    if (prioDirty) {
      await setProcessPriority(props.process.pid, {
        priorityClass: editingPrioClass.value ?? undefined,
        ioPriority: editingIo.value ?? undefined,
        memoryPriority: editingMem.value ?? undefined,
      });
      originalPrioClass.value = editingPrioClass.value;
      originalIo.value = editingIo.value;
      originalMem.value = editingMem.value;
    }

    // 3. Only persist when "Save as rule" is explicitly checked
    if (saveAsRule.value) {
      await persistAsRule(formatMask(editingMask.value), prioDirty);
    }

    emit("applied");
    emit("update:modelValue", false);
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
  } finally {
    applying.value = false;
  }
}

/**
 * Persist these settings as an "exact match" rule (new, or update an existing rule with the same name).
 * Priorities are only written when the user actually changed them — untouched fields
 * keep the rule's existing values, so a casual "tweak once" doesn't bake managed-priority
 * items into the auto-applied rule.
 */
async function persistAsRule(maskHex: string, includePriorities: boolean) {
  if (!props.process) return;
  const processName = props.process.name.replace(/\.exe$/i, "");
  const prio = includePriorities
    ? {
        priorityClass: editingPrioClass.value,
        ioPriority: editingIo.value,
        memoryPriority: editingMem.value,
      }
    : {};

  const rules = await loadAffinityRules();
  const normalize = (s: string) => s.replace(/\.exe$/i, "").toLowerCase();
  const existing: AffinityRule | undefined = rules.find(
    (r) => r.match_type === "exact" && normalize(r.process_name) === normalize(processName),
  );

  if (existing) {
    // Whole-record replacement: keep the existing rule's id / note / enabled state; only update mask / mode (+ any changed priorities)
    await updateAffinityRule({ ...existing, mask: maskHex, group_masks: groupMaskStrings.value, mode: affinityMode.value, ...prio });
  } else {
    await addAffinityRule({ processName, mask: maskHex, groupMasks: groupMaskStrings.value, mode: affinityMode.value, ...prio });
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
        <v-icon icon="mdi-bullseye" class="mr-2" />
        <span class="text-h6">{{ t('editRules') }} - {{ process.name }}</span>
        <v-chip size="small" color="primary" variant="tonal" class="ml-2">PID {{ process.pid }}</v-chip>
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" density="compact" @click="close" />
      </v-card-title>

      <v-divider />

      <v-card-text class="pa-4">
        <!-- Quick-select toolbar -->
        <div class="d-flex align-center flex-wrap mb-3">
          <span class="text-subtitle-2 mr-2">{{ t('quickSelect') }}</span>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectAll">{{ t('all') }}</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectNone">{{ t('clear') }}</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectPrimary">{{ t('primary') }}</v-btn>
          <v-btn size="small" variant="outlined" class="mr-2 mb-1" @click="selectSecondary">{{ t('secondary') }}</v-btn>
          <v-btn size="small" variant="outlined" class="mb-1" @click="resetMask">{{ t('reset') }}</v-btn>
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
          {{ t('multiGroupWarn') }}
        </v-alert>

        <!-- CCD-grouped rendering -->
        <div v-if="!topology" class="text-center text-medium-emphasis pa-4">
          {{ t('loadingTopology') }}
        </div>

        <div v-for="{ die, lps } in diesWithLps" :key="die.id" class="ccd-section mb-4">
          <div class="d-flex align-center mb-2">
            <span
              class="ccd-dot mr-2"
              :style="{ background: ccdColor(die.id) }"
            />
            <span class="text-subtitle-2">
              {{ die.is_ccd ? `CCD ${die.id}` : t('logicalDie', { id: die.id }) }}
            </span>
            <span class="text-caption text-medium-emphasis ml-2">
              {{ t('dieInfo', { cores: die.cores.length, threads: die.threads.length }) }}
            </span>
            <v-spacer />
            <v-btn
              size="x-small"
              variant="tonal"
              class="mr-1"
              :style="{ color: ccdColor(die.id) }"
              @click="selectCcd(die.id)"
            >
              {{ t('selectCcd') }}
            </v-btn>
            <v-btn size="x-small" variant="text" @click="clearCcd(die.id)">{{ t('clearCcd') }}</v-btn>
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
              :title="`LP ${lp.index} · Core ${lp.core_id} ${lp.is_smt_secondary ? t('smtSecondaryLabel') : t('smtPrimaryLabel')}`"
              @click="toggleBit(lp.index)"
            >
              {{ lp.index }}
            </button>
          </div>
        </div>

        <!-- Scheduling mode (M2: strict = hard mask / soft = CPU Sets) -->
        <div class="d-flex align-center mt-4 mb-2">
          <v-icon icon="mdi-tune-variant" size="18" class="mr-2" />
          <span class="text-subtitle-2">{{ t('ruleMode') }}</span>
        </div>
        <v-btn-toggle
          v-model="affinityMode"
          mandatory
          density="compact"
          color="primary"
          class="mb-1"
        >
          <v-btn value="strict">{{ t('modeStrict') }}</v-btn>
          <v-btn value="soft">{{ t('modeSoft') }}</v-btn>
        </v-btn-toggle>
        <div class="text-caption text-medium-emphasis">
          {{ affinityMode === 'soft' ? t('modeSoftHint') : t('modeStrictHint') }}
        </div>

        <!-- Priority settings -->
        <div class="d-flex align-center mt-4 mb-2">
          <v-icon icon="mdi-speedometer" size="18" class="mr-2" />
          <span class="text-subtitle-2">{{ t('prioritySettings') }}</span>
          <v-btn
            v-if="priorityDirty"
            size="x-small"
            variant="text"
            class="ml-2"
            @click="resetPriorities"
          >
            {{ t('resetPriorities') }}
          </v-btn>
        </div>
        <v-row dense>
          <v-col cols="12" sm="4">
            <v-select
              v-model="editingPrioClass"
              :items="prioClassItems"
              :label="t('cpuPriority')"
              :disabled="process.access_denied"
              density="compact"
              variant="outlined"
              hide-details
            />
          </v-col>
          <v-col cols="12" sm="4">
            <v-select
              v-model="editingIo"
              :items="ioPriorityItems"
              :label="t('ioPriority')"
              :disabled="process.access_denied"
              density="compact"
              variant="outlined"
              hide-details
            />
          </v-col>
          <v-col cols="12" sm="4">
            <v-select
              v-model="editingMem"
              :items="memPriorityItems"
              :label="t('memoryPriority')"
              :disabled="process.access_denied"
              density="compact"
              variant="outlined"
              hide-details
            />
          </v-col>
        </v-row>

        <!-- Explicit toggle: persist these settings as a rule? -->
        <div class="mt-4">
          <v-checkbox
            v-model="saveAsRule"
            :label="t('saveAsRule')"
            density="compact"
            hide-details
          />
          <div class="text-caption text-medium-emphasis">{{ t('saveAsRuleHint') }}</div>
        </div>
      </v-card-text>

      <v-divider />

      <v-card-actions class="pa-3">
        <span class="text-body-2 ml-2">
          {{ t('selectedPrefix') }} <strong class="text-primary">{{ selectedCount }}</strong> / {{ totalCount }} {{ t('logicalProcessors') }}
        </span>
        <span class="text-body-2 text-medium-emphasis ml-4">
          Mask: <code>{{ maskHex }}</code>
        </span>
        <v-spacer />
        <v-btn variant="text" :disabled="applying" @click="close">{{ t('cancel') }}</v-btn>
        <v-btn
          color="primary"
          variant="flat"
          :loading="applying"
          @click="apply"
        >
          {{ t('apply') }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<style scoped>
.ccd-section {
  padding: 8px 12px;
  /* Use the on-surface variable for a subtle background/border: white in dark mode, black in light, auto-adapts to both themes */
  background: rgba(var(--v-theme-on-surface), 0.02);
  border-radius: 6px;
  border: 1px solid rgba(var(--v-theme-on-surface), 0.06);
}

.ccd-dot {
  display: inline-block;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  border: 1px solid rgba(var(--v-theme-on-surface), 0.2);
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
  border: 1.5px solid var(--ccd);
  border-radius: 5px;
  background: transparent;
  color: var(--ccd);
  font-size: 13px;
  font-weight: 600;
  cursor: pointer;
  user-select: none;
  transition: background 0.12s ease, color 0.12s ease, transform 0.08s ease, border-color 0.12s ease;
  font-family: "Cascadia Code", "Consolas", monospace;
}

.cpu-box:hover:not(.unavailable) {
  background: color-mix(in srgb, var(--ccd) 22%, transparent);
  border-color: color-mix(in srgb, var(--ccd) 80%, white);
}

.cpu-box:active:not(.unavailable) {
  transform: scale(0.92);
}

.cpu-box.active {
  background: var(--ccd);
  color: #fff;
  border-color: color-mix(in srgb, var(--ccd) 80%, white);
}

.cpu-box.active:hover:not(.unavailable) {
  background: color-mix(in srgb, var(--ccd) 80%, white);
}

/* SMT secondary thread: dashed border */
.cpu-box.smt-secondary {
  border-style: dashed;
}
.cpu-box.smt-secondary.active {
  border-style: dashed;
  background: color-mix(in srgb, var(--ccd) 60%, transparent);
}

.cpu-box.unavailable {
  opacity: 0.3;
  cursor: not-allowed;
  border-color: rgba(117, 117, 117, 0.55);
  color: rgba(117, 117, 117, 0.85);
  background: transparent;
}
.cpu-box.unavailable:hover {
  background: transparent;
}
</style>
