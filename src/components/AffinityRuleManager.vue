<script setup lang="ts">
import { ref, computed, onMounted, watch } from "vue";
import type { AffinityRule } from "../api";
import {
  loadAffinityRules,
  addAffinityRule,
  updateAffinityRule,
  deleteAffinityRule,
  applyAffinityRules,
} from "../api";
import type { CpuTopology, RuleMatchType } from "../types";
import {
  parseMask,
  formatMask,
  MATCH_TYPE_OPTIONS,
  RULE_MODE_OPTIONS,
  PRIORITY_CLASS_OPTIONS,
  IO_PRIORITY_OPTIONS,
  MEMORY_PRIORITY_OPTIONS,
  priorityClassLabel,
  ioPriorityLabel,
  memoryPriorityLabel,
} from "../types";
import { useI18n } from "../i18n";
const { t } = useI18n();

const props = defineProps<{
  modelValue: boolean;
  topology: CpuTopology | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
  applied: [count: number];
}>();

// Rule list
const rules = ref<AffinityRule[]>([]);
// Rule currently being edited
const editingRule = ref<AffinityRule | null>(null);
const isEditing = ref(false);
const isNewRule = ref(false);

// Apply state
const applying = ref(false);
const errorMsg = ref<string | null>(null);
const successMsg = ref<string | null>(null);

// ---------- Dropdown options (computed to stay reactive to locale changes) ----------

const matchTypeItems = computed(() =>
  MATCH_TYPE_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
);
const modeItems = computed(() =>
  RULE_MODE_OPTIONS.map((o) => ({ title: t(o.labelKey), value: o.value })),
);

/** Priority dropdown items: a leading "Unmanaged" entry (null) followed by the available tiers */
function prioItems(options: Array<{ value: number; labelKey: string }>) {
  return [
    { title: t("prioUnmanaged"), value: null },
    ...options.map((o) => ({ title: t(o.labelKey), value: o.value })),
  ];
}
const prioClassItems = computed(() => prioItems(PRIORITY_CLASS_OPTIONS));
const ioPrioItems = computed(() => prioItems(IO_PRIORITY_OPTIONS));
const memPrioItems = computed(() => prioItems(MEMORY_PRIORITY_OPTIONS));

/** Match hint / placeholder swap with the match mode */
const matchHint = computed(() => {
  switch (editingRule.value?.match_type) {
    case "wildcard":
      return t("matchWildcardHint");
    case "path":
      return t("matchPathHint");
    default:
      return t("matchExactHint");
  }
});
const patternPlaceholder = computed(() => {
  switch (editingRule.value?.match_type) {
    case "wildcard":
      return t("patternPlaceholderWildcard");
    case "path":
      return t("patternPlaceholderPath");
    default:
      return t("ruleNamePlaceholder");
  }
});
const modeHint = computed(() =>
  editingRule.value?.mode === "soft" ? t("modeSoftHint") : t("modeStrictHint"),
);

function matchTypeLabel(mt: RuleMatchType): string {
  return t(MATCH_TYPE_OPTIONS.find((o) => o.value === mt)?.labelKey ?? "matchExact");
}
function modeLabel(mode: string): string {
  return mode === "soft" ? t("modeSoft") : t("modeStrict");
}

/** One-line summary of the priorities this rule manages (e.g. "CPU: High · IO: Low"); "-" when nothing is managed */
function prioritySummary(rule: AffinityRule): string {
  const parts: string[] = [];
  if (rule.priority_class !== null)
    parts.push(`${t("prioCpu")}: ${priorityClassLabel(rule.priority_class)}`);
  if (rule.io_priority !== null) parts.push(`${t("prioIo")}: ${ioPriorityLabel(rule.io_priority)}`);
  if (rule.memory_priority !== null)
    parts.push(`${t("prioMem")}: ${memoryPriorityLabel(rule.memory_priority)}`);
  return parts.length > 0 ? parts.join(" · ") : "-";
}

// Load rules
async function loadRules() {
  try {
    rules.value = await loadAffinityRules();
  } catch (e) {
    errorMsg.value = t("loadRulesFailed", { error: String(e) });
  }
}

/** Default values for a new rule (schema v2: exact match / strict mode / no managed priorities) */
function newRuleDraft(): AffinityRule {
  const groupMasks = props.topology
    ? Array.from({ length: props.topology.group_count }, (_, group) => {
        const selected = props.topology!.logical_processors
          .filter((lp) => lp.group === group)
          .reduce((mask, lp) => mask | (1n << BigInt(lp.group_index)), 0n);
        return formatMask(selected);
      })
    : undefined;
  return {
    id: "",
    process_name: "",
    mask: props.topology
      ? groupMasks?.[0] ?? "0xFF"
      : "0xFF",
    group_masks: groupMasks,
    enabled: true,
    created_at: Date.now() / 1000,
    note: "",
    match_type: "exact",
    mode: "strict",
    priority_class: null,
    io_priority: null,
    memory_priority: null,
  };
}

// Add a new rule
function addRule() {
  isNewRule.value = true;
  editingRule.value = newRuleDraft();
  isEditing.value = true;
}

// Edit a rule
function editRule(rule: AffinityRule) {
  isNewRule.value = false;
  editingRule.value = { ...rule };
  isEditing.value = true;
}

function hasSelectedCore(mask: string, groupMasks?: string[] | null): boolean {
  try {
    return groupMasks ? groupMasks.some((value) => parseMask(value) !== 0n) : parseMask(mask) !== 0n;
  } catch {
    return false;
  }
}

// Save the in-progress edit
async function saveEdit() {
  if (!editingRule.value) return;

  errorMsg.value = null;
  const wasNew = isNewRule.value;
  const draft = editingRule.value;

  // Validation
  if (!draft.process_name.trim()) {
    errorMsg.value = t("enterProcessName");
    return;
  }

  try {
    parseMask(draft.mask);
  } catch {
    errorMsg.value = t("invalidMask");
    return;
  }

  if (!hasSelectedCore(draft.mask, draft.group_masks)) {
    errorMsg.value = t("atLeastOneLp");
    return;
  }

  try {
    if (wasNew) {
      // Backend assigns id / created_at; the rest of the fields come from the edit form
      const newRule = await addAffinityRule({
        processName: draft.process_name.trim(),
        mask: draft.mask,
        groupMasks: draft.group_masks ?? undefined,
        note: draft.note,
        matchType: draft.match_type,
        mode: draft.mode,
        priorityClass: draft.priority_class,
        ioPriority: draft.io_priority,
        memoryPriority: draft.memory_priority,
      });
      rules.value.push(newRule);
    } else {
      // Whole-record replacement (located by id)
      const updated = await updateAffinityRule({
        ...draft,
        process_name: draft.process_name.trim(),
      });
      const idx = rules.value.findIndex((r) => r.id === updated.id);
      if (idx >= 0) {
        rules.value[idx] = updated;
      }
    }

    isEditing.value = false;
    editingRule.value = null;
    successMsg.value = wasNew ? t("ruleAdded") : t("ruleUpdated");
    setTimeout(() => (successMsg.value = null), 2000);
  } catch (e) {
    errorMsg.value = t("saveRuleFailed", { error: String(e) });
  }
}

// Cancel the in-progress edit
function cancelEdit() {
  isEditing.value = false;
  editingRule.value = null;
  errorMsg.value = null;
}

// Delete a rule (show a confirmation dialog first)
const deleteTarget = ref<AffinityRule | null>(null);

function requestRemoveRule(rule: AffinityRule) {
  deleteTarget.value = rule;
}

async function confirmRemoveRule() {
  const rule = deleteTarget.value;
  if (!rule) return;
  try {
    await deleteAffinityRule(rule.id);
    rules.value = rules.value.filter((r) => r.id !== rule.id);
    deleteTarget.value = null;
    successMsg.value = t("ruleDeleted");
    setTimeout(() => (successMsg.value = null), 2000);
  } catch (e) {
    errorMsg.value = t("deleteRuleFailed", { error: String(e) });
  }
}

// Toggle a rule's enabled state
async function toggleRule(rule: AffinityRule) {
  try {
    const updated = await updateAffinityRule({ ...rule, enabled: !rule.enabled });
    const idx = rules.value.findIndex((r) => r.id === updated.id);
    if (idx >= 0) {
      rules.value[idx] = updated;
    }
  } catch (e) {
    errorMsg.value = t("updateRuleFailed", { error: String(e) });
  }
}

// Apply every enabled rule
async function applyRules() {
  applying.value = true;
  errorMsg.value = null;

  try {
    const count = await applyAffinityRules();
    emit("applied", count);
    successMsg.value = t("appliedToProcesses", { count });
    setTimeout(() => (successMsg.value = null), 3000);
  } catch (e) {
    errorMsg.value = t("applyRulesFailed", { error: String(e) });
  } finally {
    applying.value = false;
  }
}

// Format a Unix-second timestamp for display
function formatTime(timestamp: number): string {
  return new Date(timestamp * 1000).toLocaleString();
}

// Human-readable mask preview (with selected-core count)
function maskPreview(mask: string): string {
  try {
    const parsed = parseMask(mask);
    const count = popcount(parsed);
    return `${mask} (${t("maskPreviewCores", { count })})`;
  } catch {
    return mask;
  }
}

/** Per-group mask editor uses a compact comma-separated form, preserving
 * group order (group 0 first). The backend is the authority for validation. */
const groupMasksText = computed({
  get: () => editingRule.value?.group_masks?.join(", ") ?? "",
  set: (value: string) => {
    if (!editingRule.value) return;
    const masks = value.split(",").map((mask) => mask.trim()).filter(Boolean);
    editingRule.value.group_masks = masks.length ? masks : null;
    if (masks[0]) editingRule.value.mask = masks[0];
  },
});

function popcount(n: bigint): number {
  let count = 0;
  while (n > 0n) {
    count += Number(n & 1n);
    n >>= 1n;
  }
  return count;
}

// Watch the dialog open/close state
watch(
  () => props.modelValue,
  (val) => {
    if (val) {
      loadRules();
    }
  },
);

onMounted(() => {
  if (props.modelValue) {
    loadRules();
  }
});
</script>

<template>
  <!-- Main rule manager dialog -->
  <v-dialog
    :model-value="modelValue"
    @update:model-value="emit('update:modelValue', $event)"
    max-width="1020"
  >
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-bullseye" class="mr-2" />
        {{ t('ruleManagerTitle') }}
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" @click="emit('update:modelValue', false)" />
      </v-card-title>

      <v-card-text>
        <!-- Inline status messages -->
        <v-alert v-if="errorMsg" type="error" density="compact" class="mb-3" closable @click:close="errorMsg = null">
          {{ errorMsg }}
        </v-alert>
        <v-alert v-if="successMsg" type="success" density="compact" class="mb-3">
          {{ successMsg }}
        </v-alert>

        <!-- Action buttons -->
        <div class="d-flex gap-2 mb-4">
          <v-btn color="primary" prepend-icon="mdi-plus" @click="addRule">
            {{ t('addRule') }}
          </v-btn>
          <v-btn
            color="success"
            prepend-icon="mdi-play"
            :loading="applying"
            :disabled="rules.length === 0"
            @click="applyRules"
          >
            {{ t('applyRules') }}
          </v-btn>
          <v-spacer />
          <v-chip v-if="rules.length > 0" color="info" variant="outlined">
            {{ t('rulesEnabled', { enabled: rules.filter(r => r.enabled).length, total: rules.length }) }}
          </v-chip>
        </div>

        <!-- Rule list -->
        <v-table v-if="rules.length > 0" density="compact">
          <thead>
            <tr>
              <th>{{ t('colEnabled') }}</th>
              <th>{{ t('colProcessName') }}</th>
              <th>{{ t('colMask') }}</th>
              <th>{{ t('ruleMode') }}</th>
              <th>{{ t('priority') }}</th>
              <th>{{ t('colNote') }}</th>
              <th>{{ t('colCreatedAt') }}</th>
              <th>{{ t('colActions') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="rule in rules" :key="rule.id">
              <td>
                <v-switch
                  :model-value="rule.enabled"
                  @update:model-value="toggleRule(rule)"
                  hide-details
                  density="compact"
                  color="primary"
                />
              </td>
              <td>
                <code class="text-primary">{{ rule.process_name }}</code>
                <v-chip
                  v-if="rule.match_type !== 'exact'"
                  size="x-small"
                  variant="tonal"
                  class="ml-1"
                >
                  {{ matchTypeLabel(rule.match_type) }}
                </v-chip>
              </td>
              <td>
                <v-chip size="small" variant="outlined">
                  {{ maskPreview(rule.group_masks?.join(", ") ?? rule.mask) }}
                </v-chip>
              </td>
              <td>
                <v-chip
                  size="x-small"
                  :variant="rule.mode === 'soft' ? 'tonal' : 'outlined'"
                  :color="rule.mode === 'soft' ? 'teal' : undefined"
                >
                  {{ modeLabel(rule.mode) }}
                </v-chip>
              </td>
              <td class="text-caption">{{ prioritySummary(rule) }}</td>
              <td>{{ rule.note || "-" }}</td>
              <td class="text-caption">{{ formatTime(rule.created_at) }}</td>
              <td>
                <v-btn icon="mdi-pencil" size="small" variant="text" @click="editRule(rule)" />
                <v-btn icon="mdi-delete" size="small" variant="text" color="error" @click="requestRemoveRule(rule)" />
              </td>
            </tr>
          </tbody>
        </v-table>

        <!-- Empty state -->
        <v-alert v-else type="info" variant="tonal" class="mt-4">
          {{ t('noRules') }}
          <br>
          <small>{{ t('noRulesHint') }}</small>
        </v-alert>

        <!-- Usage notes -->
        <v-expansion-panels class="mt-4">
          <v-expansion-panel>
            <v-expansion-panel-title>
              <v-icon icon="mdi-information" class="mr-2" />
              {{ t('usageTitle') }}
            </v-expansion-panel-title>
            <v-expansion-panel-text>
              <ul class="text-body-2">
                <li><strong>{{ t('usageNameLabel') }}</strong>{{ t('usageNameDesc') }}</li>
                <li><strong>{{ t('usageMatchLabel') }}</strong>{{ t('usageMatchDesc') }}</li>
                <li><strong>{{ t('usageMaskLabel') }}</strong>{{ t('usageMaskDesc') }}</li>
                <li><strong>{{ t('usageModeLabel') }}</strong>{{ t('usageModeDesc') }}</li>
                <li><strong>{{ t('usagePrioLabel') }}</strong>{{ t('usagePrioDesc') }}</li>
                <li><strong>{{ t('usageApplyLabel') }}</strong>{{ t('usageApplyDesc') }}</li>
                <li><strong>{{ t('usageAutoLabel') }}</strong>{{ t('usageAutoDesc') }}</li>
              </ul>
            </v-expansion-panel-text>
          </v-expansion-panel>
        </v-expansion-panels>
      </v-card-text>

      <v-card-actions>
        <v-spacer />
        <v-btn @click="emit('update:modelValue', false)">{{ t('close') }}</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Edit dialog (separate from the main dialog) -->
  <v-dialog v-model="isEditing" max-width="640">
    <v-card>
      <v-card-title>{{ isNewRule ? t('addRule') : t('editRules') }}</v-card-title>
      <v-card-text>
        <!-- The match mode determines what process_name means (name / wildcard / path) -->
        <v-select
          v-if="editingRule"
          v-model="editingRule.match_type"
          :items="matchTypeItems"
          :label="t('matchType')"
          :hint="matchHint"
          persistent-hint
          density="compact"
          variant="outlined"
          class="mb-3"
        />

        <v-text-field
          v-if="topology && topology.group_count > 1"
          v-model="groupMasksText"
          :label="t('groupMasks')"
          :hint="t('groupMasksHint')"
          persistent-hint
          density="compact"
          class="mt-3"
        />

        <v-text-field
          v-if="editingRule"
          v-model="editingRule.process_name"
          :label="t('ruleNameLabel')"
          :placeholder="patternPlaceholder"
          class="mb-3"
        />

        <v-text-field
          v-if="editingRule"
          v-model="editingRule.mask"
          :label="t('ruleMaskLabel')"
          placeholder="0xFF"
          :hint="t('ruleMaskHint')"
          persistent-hint
          class="mb-3"
        />

        <!-- Scheduling mode: strict hard mask / elastic CPU Sets -->
        <v-select
          v-if="editingRule"
          v-model="editingRule.mode"
          :items="modeItems"
          :label="t('ruleMode')"
          :hint="modeHint"
          persistent-hint
          density="compact"
          variant="outlined"
          class="mb-3"
        />

        <!-- Managed priorities: null = don't touch -->
        <template v-if="editingRule">
          <div class="text-subtitle-2 mb-1">{{ t('rulePriorityLabel') }}</div>
          <div class="text-caption text-medium-emphasis mb-2">{{ t('rulePriorityHint') }}</div>
          <v-row dense>
            <v-col cols="12" sm="4">
              <v-select
                v-model="editingRule.priority_class"
                :items="prioClassItems"
                :label="t('prioCpu')"
                density="compact"
                variant="outlined"
                hide-details
              />
            </v-col>
            <v-col cols="12" sm="4">
              <v-select
                v-model="editingRule.io_priority"
                :items="ioPrioItems"
                :label="t('prioIo')"
                density="compact"
                variant="outlined"
                hide-details
              />
            </v-col>
            <v-col cols="12" sm="4">
              <v-select
                v-model="editingRule.memory_priority"
                :items="memPrioItems"
                :label="t('prioMem')"
                density="compact"
                variant="outlined"
                hide-details
              />
            </v-col>
          </v-row>
        </template>

        <v-text-field
          v-if="editingRule"
          v-model="editingRule.note"
          :label="t('ruleNoteLabel')"
          :placeholder="t('ruleNotePlaceholder')"
          class="mb-3 mt-3"
        />

        <v-alert v-if="errorMsg" type="error" density="compact" class="mt-3">
          {{ errorMsg }}
        </v-alert>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn @click="cancelEdit">{{ t('cancel') }}</v-btn>
        <v-btn color="primary" :disabled="!editingRule || !hasSelectedCore(editingRule.mask, editingRule.group_masks)" @click="saveEdit">
          {{ isNewRule ? t('add') : t('save') }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Delete-confirmation dialog -->
  <v-dialog :model-value="deleteTarget !== null" @update:model-value="deleteTarget = null" max-width="420">
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-delete" class="mr-2" color="error" />
        <span class="text-h6">{{ t('confirmDeleteTitle') }}</span>
      </v-card-title>
      <v-card-text class="text-body-2">
        {{ t('confirmDeleteRule') }}
        <div v-if="deleteTarget" class="mt-2">
          <code class="text-error">{{ deleteTarget.process_name }}</code>
        </div>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="deleteTarget = null">{{ t('cancel') }}</v-btn>
        <v-btn color="error" variant="flat" prepend-icon="mdi-delete" @click="confirmRemoveRule">
          {{ t('delete') }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<style scoped>
code {
  font-family: "Cascadia Code", "Consolas", monospace;
}
</style>
