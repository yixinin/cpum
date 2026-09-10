<script setup lang="ts">
import { ref, computed, onMounted, watch } from "vue";
import type { AffinityRule } from "../api";
import {
  loadAffinityRules,
  deleteAffinityRule,
  applyAffinityRules,
  updateAffinityRule,
} from "../api";
import type { CpuTopology, RuleMatchType } from "../types";
import {
  parseMask,
  formatMask,
  MATCH_TYPE_OPTIONS,
  RULE_MODE_OPTIONS,
  priorityClassLabel,
  ioPriorityLabel,
  memoryPriorityLabel,
} from "../types";
import AffinityEditor, { type EditorTarget } from "./AffinityEditor.vue";
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
// The actual editing UI is provided by the shared AffinityEditor; the rule
// manager only owns the draft (or the existing rule) and the lifecycle.
const editorTarget = ref<EditorTarget | null>(null);

// Apply state
const applying = ref(false);
const errorMsg = ref<string | null>(null);
const successMsg = ref<string | null>(null);

// Bridge between AffinityEditor's v-model (boolean) and our editorTarget ref:
// the editor is "open" iff a target is set. Using a computed writable keeps
// the close path single-sourced (close -> target = null -> editorOpen -> false).
const editorOpen = computed<boolean>({
  get: () => editorTarget.value !== null,
  set: (val) => {
    if (!val) editorTarget.value = null;
  },
});

// ---------- Helpers (used by the list rows) ----------
function matchTypeLabel(mt: RuleMatchType): string {
  const opt = MATCH_TYPE_OPTIONS.find((o) => o.value === mt);
  return opt ? t(opt.labelKey) : mt;
}
function modeLabel(mode: string): string {
  const opt = RULE_MODE_OPTIONS.find((o) => o.value === mode);
  return opt ? t(opt.labelKey) : mode;
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
    mask: props.topology ? groupMasks?.[0] ?? "0xFF" : "0xFF",
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

// Open the shared editor in "new rule" mode
function addRule() {
  editorTarget.value = { kind: "rule", rule: newRuleDraft(), isNew: true };
}
// Open the shared editor in "edit existing rule" mode
function editRule(rule: AffinityRule) {
  editorTarget.value = { kind: "rule", rule: { ...rule }, isNew: false };
}

/** Splice the saved rule into the local list and flash a success toast. */
function onRuleSaved(saved: AffinityRule) {
  const idx = rules.value.findIndex((r) => r.id === saved.id);
  if (idx >= 0) {
    rules.value[idx] = saved;
    successMsg.value = t("ruleUpdated");
  } else {
    rules.value.push(saved);
    successMsg.value = t("ruleAdded");
  }
  setTimeout(() => (successMsg.value = null), 2000);
  editorTarget.value = null;
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

  <!-- Shared affinity editor: drives the rule manager's Add / Edit actions
       (rule mode) and the process-list right-click "Edit Rule" (process mode). -->
  <AffinityEditor
    v-model="editorOpen"
    :target="editorTarget"
    :topology="topology"
    @saved="onRuleSaved"
  />

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
