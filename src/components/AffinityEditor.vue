<script setup lang="ts">
import { ref, computed, watch } from "vue";
import type {
  CpuTopology,
  ProcessInfo,
  LogicalProcessorInfo,
  RuleMode,
  RuleMatchType,
} from "../types";
import {
  parseMask,
  formatMask,
  popcount,
  getBit,
  setBit,
  MATCH_TYPE_OPTIONS,
  PRIORITY_CLASS_OPTIONS,
  IO_PRIORITY_OPTIONS,
  MEMORY_PRIORITY_OPTIONS,
} from "../types";
import type { AffinityRule } from "../api";
import {
  setProcessAffinity,
  setProcessPriority,
  loadAffinityRules,
  addAffinityRule,
  updateAffinityRule,
} from "../api";
import { useI18n } from "../i18n";
const { t } = useI18n();

/**
 * Editor target. Two distinct flows reuse the same UI:
 *  - "process": edit a live process; the Save button writes to the process and
 *    optionally persists the same settings as a rule (controlled by `saveAsRule`).
 *  - "rule":    create or edit a persisted AffinityRule. The Save button writes
 *    to the rule file (add or update by id).
 */
export type EditorTarget =
  | { kind: "process"; process: ProcessInfo }
  | { kind: "rule"; rule: AffinityRule; isNew: boolean };

const props = defineProps<{
  modelValue: boolean;
  target: EditorTarget | null;
  topology: CpuTopology | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
  /** Fired when a process-mode save succeeds (parent may refresh the process list). */
  applied: [];
  /** Fired when a rule-mode save succeeds (parent should splice the result into its rule list). */
  saved: [rule: AffinityRule];
}>();

// ---------- Mode flags ----------
const isProcessMode = computed(() => props.target?.kind === "process");
const isRuleMode = computed(() => props.target?.kind === "rule");
const isNewRule = computed(() => isRuleMode.value && props.target?.kind === "rule" && props.target.isNew);

// ---------- Mask state ----------
const editingMask = ref<bigint>(0n);
const originalMask = ref<bigint>(0n);
const systemMask = ref<bigint>(0n);
const applying = ref(false);
const errorMsg = ref<string | null>(null);

// ---------- Priority state ----------
const editingPrioClass = ref<number | null>(null);
const editingIo = ref<number | null>(null);
const editingMem = ref<number | null>(null);
const originalPrioClass = ref<number | null>(null);
const originalIo = ref<number | null>(null);
const originalMem = ref<number | null>(null);

// ---------- Scheduling mode (shared by both flows) ----------
const affinityMode = ref<RuleMode>("strict");

// ---------- Process-mode only: optional rule persistence ----------
const saveAsRule = ref(false);

// ---------- Rule-mode only fields ----------
const rulePattern = ref("");
const ruleMatchType = ref<RuleMatchType>("exact");
const ruleNote = ref("");

// Initialize / reset when the dialog opens or the target changes
watch(
  () => [props.modelValue, props.target],
  () => {
    if (!props.modelValue || !props.target) return;
    if (props.target.kind === "process") {
      const p = props.target.process;
      originalMask.value = p.group_affinity_masks
        ? p.group_affinity_masks.reduce(
            (all, value, group) => all | (parseMask(value) << BigInt(group * 64)),
            0n,
          )
        : parseMask(p.affinity_mask);
      editingMask.value = originalMask.value;
      // Restrict selectable LPs to the process's own system affinity (the OS won't let us pin outside it)
      systemMask.value = p.group_system_affinity_masks
        ? p.group_system_affinity_masks.reduce(
            (all, value, group) => all | (parseMask(value) << BigInt(group * 64)),
            0n,
          )
        : parseMask(p.system_affinity_mask);
      originalPrioClass.value = p.priority_class;
      editingPrioClass.value = p.priority_class;
      originalIo.value = p.io_priority;
      editingIo.value = p.io_priority;
      originalMem.value = p.memory_priority;
      editingMem.value = p.memory_priority;
      affinityMode.value = "strict";
      saveAsRule.value = false;
    } else {
      const r = props.target.rule;
      // Combine the rule's per-group masks into a single bigint so the CCD grid and the
      // group-masks text field can drive the same editing state.
      originalMask.value = r.group_masks
        ? r.group_masks.reduce(
            (all, value, group) => all | (parseMask(value) << BigInt(group * 64)),
            0n,
          )
        : parseMask(r.mask);
      editingMask.value = originalMask.value;
      // In rule mode the system mask is the full topology — the rule file is per-system,
      // so we let the user pick any LP here and let the backend validate on apply.
      systemMask.value = props.topology
        ? props.topology.logical_processors.reduce(
            (all, lp) => all | (1n << BigInt(lp.index)),
            0n,
          )
        : 0n;
      editingPrioClass.value = r.priority_class;
      originalPrioClass.value = r.priority_class;
      editingIo.value = r.io_priority;
      originalIo.value = r.io_priority;
      editingMem.value = r.memory_priority;
      originalMem.value = r.memory_priority;
      affinityMode.value = r.mode;
      rulePattern.value = r.process_name;
      ruleMatchType.value = r.match_type;
      ruleNote.value = r.note;
      saveAsRule.value = false;
    }
    errorMsg.value = null;
  },
  { immediate: true },
);

// ---------- Dropdown options ----------
// Rule mode adds a leading "Unmanaged" entry (null) since a rule can opt out of
// managing a given priority; process mode mirrors the original behavior.
function prioItems(options: Array<{ value: number; labelKey: string }>) {
  const items = options.map((o) => ({ title: t(o.labelKey), value: o.value }));
  if (isRuleMode.value) items.unshift({ title: t("prioUnmanaged"), value: null as unknown as number });
  return items;
}
const prioClassItems = computed(() => prioItems(PRIORITY_CLASS_OPTIONS));
const ioPriorityItems = computed(() => prioItems(IO_PRIORITY_OPTIONS));
const memPriorityItems = computed(() => prioItems(MEMORY_PRIORITY_OPTIONS));

const matchTypeItems = computed(() =>
  MATCH_TYPE_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
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

// ---------- Pattern / match / note helpers (rule mode) ----------
const matchHint = computed(() => {
  switch (ruleMatchType.value) {
    case "wildcard": return t("matchWildcardHint");
    case "path": return t("matchPathHint");
    default: return t("matchExactHint");
  }
});
const patternPlaceholder = computed(() => {
  switch (ruleMatchType.value) {
    case "wildcard": return t("patternPlaceholderWildcard");
    case "path": return t("patternPlaceholderPath");
    default: return t("ruleNamePlaceholder");
  }
});
const modeHint = computed(() =>
  affinityMode.value === "soft" ? t("modeSoftHint") : t("modeStrictHint"),
);

// ---------- CCD rendering ----------
const lpMap = computed(() => {
  const m = new Map<number, LogicalProcessorInfo>();
  if (props.topology) {
    for (const lp of props.topology.logical_processors) m.set(lp.index, lp);
  }
  return m;
});
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

/** Multi-group text field. Two-way bound to `editingMask` so the CCD grid and the
 *  text view stay in sync. The text takes whichever value the user touched last. */
const groupMasksText = computed({
  get: () => groupMaskStrings.value.join(", "),
  set: (value: string) => {
    const masks = value.split(",").map((m) => m.trim()).filter(Boolean);
    if (masks.length === 0) {
      editingMask.value = 0n;
      return;
    }
    let combined = 0n;
    for (let group = 0; group < masks.length; group++) {
      combined |= parseMask(masks[group]) << BigInt(group * 64);
    }
    editingMask.value = combined;
  },
});

// ---------- CCD color palette (up to 8 CCDs, high-contrast) ----------
const CCD_COLORS = [
  "#42A5F5",
  "#66BB6A",
  "#FFA726",
  "#EF5350",
  "#AB47BC",
  "#26C6DA",
  "#FFEE58",
  "#8D6E63",
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
    if (lp.smt_thread_id === 0 && isAvailable(lp.index)) m = setBit(m, lp.index, true);
  }
  editingMask.value = m;
}
function selectSecondary() {
  if (!props.topology) return;
  let m = 0n;
  for (const lp of props.topology.logical_processors) {
    if (lp.smt_thread_id > 0 && isAvailable(lp.index)) m = setBit(m, lp.index, true);
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
  for (const bit of die.threads) if (isAvailable(bit)) m = setBit(m, bit, true);
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

// ---------- Save / apply ----------
function validateAndCollect(): { ok: true; mask: string; prioDirty: boolean } | { ok: false } {
  if (editingMask.value === 0n) {
    errorMsg.value = t("atLeastOneLp");
    return { ok: false };
  }
  if (isRuleMode.value && !rulePattern.value.trim()) {
    errorMsg.value = t("enterProcessName");
    return { ok: false };
  }
  return {
    ok: true,
    mask: formatMask(editingMask.value),
    prioDirty: priorityDirty.value,
  };
}

async function apply() {
  if (!props.target) return;
  const check = validateAndCollect();
  if (!check.ok) return;
  applying.value = true;
  errorMsg.value = null;

  try {
    if (props.target.kind === "process") {
      await applyToProcess(props.target.process.pid, check.mask, check.prioDirty);
      emit("applied");
    } else if (props.target.isNew) {
      const saved = await addAffinityRule({
        processName: rulePattern.value.trim(),
        mask: check.mask,
        groupMasks: props.topology && props.topology.group_count > 1 ? groupMaskStrings.value : undefined,
        note: ruleNote.value,
        matchType: ruleMatchType.value,
        mode: affinityMode.value,
        priorityClass: editingPrioClass.value,
        ioPriority: editingIo.value,
        memoryPriority: editingMem.value,
      });
      emit("saved", saved);
    } else {
      // Whole-record replacement; backend locates by id
      const updated = await updateAffinityRule({
        ...props.target.rule,
        process_name: rulePattern.value.trim(),
        mask: check.mask,
        group_masks: props.topology && props.topology.group_count > 1 ? groupMaskStrings.value : null,
        match_type: ruleMatchType.value,
        mode: affinityMode.value,
        priority_class: editingPrioClass.value,
        io_priority: editingIo.value,
        memory_priority: editingMem.value,
        note: ruleNote.value,
      });
      emit("saved", updated);
    }
    emit("update:modelValue", false);
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
  } finally {
    applying.value = false;
  }
}

/** Process-mode save: write the live process, optionally persist as rule. */
async function applyToProcess(pid: number, mask: string, prioDirty: boolean) {
  await setProcessAffinity(pid, editingMask.value, affinityMode.value, groupMaskStrings.value);
  originalMask.value = editingMask.value;
  if (prioDirty) {
    await setProcessPriority(pid, {
      priorityClass: editingPrioClass.value ?? undefined,
      ioPriority: editingIo.value ?? undefined,
      memoryPriority: editingMem.value ?? undefined,
    });
    originalPrioClass.value = editingPrioClass.value;
    originalIo.value = editingIo.value;
    originalMem.value = editingMem.value;
  }
  if (saveAsRule.value) {
    await persistAsRule(mask, prioDirty);
  }
}

/** Persist the current settings as an exact-match rule (new or update existing by name). */
async function persistAsRule(maskHex: string, includePriorities: boolean) {
  if (!props.target || props.target.kind !== "process") return;
  const processName = props.target.process.name.replace(/\.exe$/i, "");
  const prio = includePriorities
    ? {
        priorityClass: editingPrioClass.value,
        ioPriority: editingIo.value,
        memoryPriority: editingMem.value,
      }
    : {};
  const rules = await loadAffinityRules();
  const normalize = (s: string) => s.replace(/\.exe$/i, "").toLowerCase();
  const existing = rules.find(
    (r) => r.match_type === "exact" && normalize(r.process_name) === normalize(processName),
  );
  if (existing) {
    await updateAffinityRule({ ...existing, mask: maskHex, group_masks: groupMaskStrings.value, mode: affinityMode.value, ...prio });
  } else {
    await addAffinityRule({ processName, mask: maskHex, groupMasks: groupMaskStrings.value, mode: affinityMode.value, ...prio });
  }
}

function close() {
  emit("update:modelValue", false);
}

/** Primary button label: "Save" / "Add" (rule mode) or "Apply" (process mode). */
const primaryLabel = computed(() => {
  if (isRuleMode.value) return isNewRule.value ? t("add") : t("save");
  return t("apply");
});
</script>

<template>
  <v-dialog
    :model-value="modelValue"
    max-width="920"
    persistent
    scroll-strategy="block"
    @update:model-value="emit('update:modelValue', $event)"
  >
    <v-card v-if="target">
      <v-card-title class="d-flex align-center pa-3">
        <v-icon icon="mdi-bullseye" class="mr-2" />
        <!-- Title: process editor keeps the original "Edit Rules - <name>"; rule editor shows a single-rule title -->
        <span v-if="isProcessMode" class="text-h6">
          {{ t('editRules') }} - {{ (target as Extract<EditorTarget, {kind:'process'}>).process.name }}
        </span>
        <span v-else-if="isNewRule" class="text-h6">{{ t('addRule') }}</span>
        <span v-else class="text-h6">{{ t('editRules') }}</span>

        <v-chip
          v-if="isProcessMode"
          size="small"
          color="primary"
          variant="tonal"
          class="ml-2"
        >PID {{ (target as Extract<EditorTarget, {kind:'process'}>).process.pid }}</v-chip>
        <v-chip
          v-else-if="!isNewRule"
          size="small"
          color="primary"
          variant="tonal"
          class="ml-2"
        >{{ (target as Extract<EditorTarget, {kind:'rule'}>).rule.id.slice(0, 8) }}</v-chip>

        <v-spacer />
        <v-btn icon="mdi-close" variant="text" density="compact" @click="close" />
      </v-card-title>

      <v-divider />

      <v-card-text class="pa-4">
        <!-- Rule-only fields: match type + pattern + note. Shown at the top so the user
             sets the rule's identity before the visual mask editor. -->
        <template v-if="isRuleMode">
          <v-select
            v-model="ruleMatchType"
            :items="matchTypeItems"
            :label="t('matchType')"
            :hint="matchHint"
            persistent-hint
            density="compact"
            variant="outlined"
            class="mb-3"
          />

          <v-text-field
            v-model="rulePattern"
            :label="t('ruleNameLabel')"
            :placeholder="patternPlaceholder"
            class="mb-3"
            density="compact"
            variant="outlined"
          />

          <!-- Per-group mask editor (only meaningful when the system spans multiple groups) -->
          <v-text-field
            v-if="topology && topology.group_count > 1"
            v-model="groupMasksText"
            :label="t('groupMasks')"
            :hint="t('groupMasksHint')"
            persistent-hint
            density="compact"
            variant="outlined"
            class="mb-3"
          />
        </template>

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
          v-if="isProcessMode && topology && !topology.single_group"
          type="warning"
          density="compact"
          class="mb-3"
        >
          {{ t('multiGroupWarn') }}
        </v-alert>

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
          {{ modeHint }}
        </div>

        <!-- Priority settings -->
        <div class="d-flex align-center mt-4 mb-2">
          <v-icon icon="mdi-speedometer" size="18" class="mr-2" />
          <span class="text-subtitle-2">{{ t('prioritySettings') }}</span>
          <v-btn
            v-if="isProcessMode && priorityDirty"
            size="x-small"
            variant="text"
            class="ml-2"
            @click="resetPriorities"
          >
            {{ t('resetPriorities') }}
          </v-btn>
        </div>
        <div v-if="isRuleMode" class="text-caption text-medium-emphasis mb-2">
          {{ t('rulePriorityHint') }}
        </div>
        <v-row dense>
          <v-col cols="12" sm="4">
            <v-select
              v-model="editingPrioClass"
              :items="prioClassItems"
              :label="t('cpuPriority')"
              :disabled="isProcessMode && (target as Extract<EditorTarget, {kind:'process'}>).process.access_denied"
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
              :disabled="isProcessMode && (target as Extract<EditorTarget, {kind:'process'}>).process.access_denied"
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
              :disabled="isProcessMode && (target as Extract<EditorTarget, {kind:'process'}>).process.access_denied"
              density="compact"
              variant="outlined"
              hide-details
            />
          </v-col>
        </v-row>

        <!-- Note field (rule mode only) -->
        <v-text-field
          v-if="isRuleMode"
          v-model="ruleNote"
          :label="t('ruleNoteLabel')"
          :placeholder="t('ruleNotePlaceholder')"
          density="compact"
          variant="outlined"
          class="mt-3"
        />

        <!-- Process-mode only: optional persistence to a rule -->
        <div v-if="isProcessMode" class="mt-4">
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
          {{ primaryLabel }}
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
