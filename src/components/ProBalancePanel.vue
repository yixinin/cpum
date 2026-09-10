<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from "vue";
import type { PbLogEntry, PbStatistics, PbStatus, ProBalanceConfig } from "../types";
import { priorityClassLabel, ioPriorityLabel } from "../types";
import { getProBalanceConfig, saveProBalanceConfig, getProBalanceStatus, getProBalanceLog, getProBalanceStatistics } from "../api";
import { useI18n } from "../i18n";
const { t } = useI18n();

const props = defineProps<{
  modelValue: boolean;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
}>();

// ---------- State ----------
const config = ref<ProBalanceConfig | null>(null);
const status = ref<PbStatus | null>(null);
const logs = ref<PbLogEntry[]>([]);
const statistics = ref<PbStatistics | null>(null);
const loading = ref(false);
const errorMsg = ref<string | null>(null);
/** True once the initial config load has populated `config`. The auto-save watcher
 *  checks this to skip the load-induced assignment (otherwise every open would
 *  immediately write the just-loaded data back to the backend). */
const configLoaded = ref(false);

/** Status polling handle (refreshes status + log every 2s while the panel is open) */
let pollTimer: ReturnType<typeof setInterval> | null = null;
/** If the status file is older than this many seconds, the service is considered offline */
const STATUS_FRESH_SECS = 5;
/** Debounce window for auto-save: collapse rapid keystrokes / toggle clicks into
 *  a single backend write (e.g. typing "1000" doesn't fire 4 IPC calls). */
const AUTOSAVE_DEBOUNCE_MS = 400;
let saveTimer: ReturnType<typeof setTimeout> | null = null;

// ---------- Field metadata (drives the v-for in the template) ----------
type NumericConfigKey = "fg_cpu_threshold" | "bg_cpu_threshold" | "sustain_secs" | "restore_after_secs" | "max_downgrade_secs";

interface FieldSpec {
  key: NumericConfigKey;
  labelKey: string;
  hintKey: string;
  /** Unit shown as the v-text-field suffix (e.g. "%", "s") — the field labels intentionally drop this to stay short */
  suffix: string;
  min: number;
  max: number;
}

const thresholdFields: FieldSpec[] = [
  { key: "fg_cpu_threshold", labelKey: "pbFgThreshold", hintKey: "pbFgThresholdHint", suffix: "%", min: 10, max: 10000 },
  { key: "bg_cpu_threshold", labelKey: "pbBgThreshold", hintKey: "pbBgThresholdHint", suffix: "%", min: 1, max: 10000 },
];

const timingFields: FieldSpec[] = [
  { key: "sustain_secs", labelKey: "pbSustain", hintKey: "pbSustainHint", suffix: "s", min: 1, max: 600 },
  { key: "restore_after_secs", labelKey: "pbRestoreAfter", hintKey: "pbRestoreAfterHint", suffix: "s", min: 1, max: 3600 },
  { key: "max_downgrade_secs", labelKey: "pbMaxDowngrade", hintKey: "pbMaxDowngradeHint", suffix: "s", min: 30, max: 86400 },
];

// ---------- Derived engine state ----------

/** Whether the service is online (based on status-file freshness) */
const serviceOnline = computed(() => {
  if (!status.value) return false;
  return Date.now() / 1000 - status.value.ts < STATUS_FRESH_SECS;
});

/** Engine runtime state: disabled / engaged / idle / offline */
const engineState = computed<"disabled" | "engaged" | "idle" | "offline">(() => {
  if (!status.value || !serviceOnline.value) return "offline";
  if (!status.value.enabled) return "disabled";
  return status.value.engaged ? "engaged" : "idle";
});

const engineStateColor = computed(
  () =>
    ({
      engaged: "warning",
      idle: "success",
      disabled: "grey",
      offline: "error",
    })[engineState.value],
);

const engineStateLabel = computed(
  () =>
    ({
      engaged: t("pbEngaged"),
      idle: t("pbIdle"),
      disabled: t("pbDisabled"),
      offline: t("pbServiceOffline"),
    })[engineState.value],
);

/** Whether the live status detail line should render. Collapses to nothing when
 *  the engine is idle/disabled/offline, so the header stays minimal. */
const showStatusDetails = computed(
  () =>
    (status.value?.engaged && status.value.downgraded > 0) ||
    (serviceOnline.value && status.value?.fg_pid) ||
    !!status.value?.game_mode_active,
);

// ---------- Log display helpers ----------

function actionLabel(action: string): string {
  switch (action) {
    case "downgrade": return t("pbActionDowngrade");
    case "restore": return t("pbActionRestore");
    case "exit": return t("pbActionExit");
    case "game_mode_boost": return t("pbActionGameBoost");
    case "game_mode_restore": return t("pbActionGameRestore");
    default: return action;
  }
}

function actionColor(action: string): string {
  switch (action) {
    case "downgrade": return "warning";
    case "restore": return "success";
    default: return "grey";
  }
}

function reasonLabel(reason: string | null): string {
  switch (reason) {
    case "contention_cleared": return t("pbReasonContentionCleared");
    case "timeout": return t("pbReasonTimeout");
    case "process_exited": return t("pbReasonProcessExited");
    case "disabled": return t("pbReasonDisabled");
    case "shutdown": return t("pbReasonShutdown");
    case "fullscreen_ended": return t("pbReasonFullscreenEnded");
    case "foreground_changed": return t("pbReasonForegroundChanged");
    case "startup_reconcile": return t("pbReasonStartupReconcile");
    default: return "";
  }
}

/** Compact priority change: just the CPU class ("Normal → Below Normal"). The full
 *  breakdown (IO / memory) lives in `priorityFull` and is exposed via tooltip. */
function priorityShort(entry: PbLogEntry): string {
  const from = entry.from ? priorityClassLabel(entry.from.priority_class) : "";
  const to = entry.to ? priorityClassLabel(entry.to.priority_class) : "";
  if (!from && !to) return "-";
  return `${from || "-"} → ${to || "-"}`;
}

function priorityFull(entry: PbLogEntry): string {
  const from = entry.from
    ? `${priorityClassLabel(entry.from.priority_class)} / IO ${ioPriorityLabel(entry.from.io_priority)}`
    : "";
  const to = entry.to
    ? `${priorityClassLabel(entry.to.priority_class)} / IO ${ioPriorityLabel(entry.to.io_priority)}`
    : "";
  if (!from && !to) return "-";
  return `${from || "-"} → ${to || "-"}`;
}

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

// ---------- Load / save / poll ----------

async function loadConfig() {
  loading.value = true;
  try {
    config.value = await getProBalanceConfig();
    configLoaded.value = true;
    errorMsg.value = null;
  } catch (e) {
    errorMsg.value = t("pbLoadFailed", { error: String(e) });
  } finally {
    loading.value = false;
  }
}

async function poll() {
  try {
    status.value = await getProBalanceStatus();
  } catch {
    status.value = null;
  }
  try {
    logs.value = await getProBalanceLog(50);
  } catch {
    // Log read failures are silenced (the service may have just started)
  }
  try { statistics.value = await getProBalanceStatistics(); } catch { statistics.value = null; }
}

/** Sanity check for numeric inputs (returns an error message, or null when valid).
 *  Range validation is the backend's job — cpum-core config.rs is the single authority. */
function validateConfig(c: ProBalanceConfig): string | null {
  const numbers = [c.fg_cpu_threshold, c.bg_cpu_threshold, c.sustain_secs, c.restore_after_secs, c.max_downgrade_secs];
  if (numbers.some((n) => !Number.isFinite(n))) {
    return t("pbValidateNumeric");
  }
  return null;
}

async function save() {
  if (!config.value) return;
  // Reject non-numeric input early; range rules are enforced by the backend on save
  const problem = validateConfig(config.value);
  if (problem) {
    errorMsg.value = problem;
    return;
  }
  try {
    await saveProBalanceConfig(config.value);
    errorMsg.value = null;
  } catch (e) {
    errorMsg.value = t("pbSaveFailed", { error: String(e) });
  }
}

/** Auto-save: any field change writes the whole config back after a short
 *  debounce. The backend hot-reloads within ~1s, so the change is effectively
 *  immediate from the user's perspective. */
function scheduleAutoSave() {
  if (saveTimer !== null) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    saveTimer = null;
    void save();
  }, AUTOSAVE_DEBOUNCE_MS);
}

/** Run a pending auto-save synchronously (used when the dialog closes so the
 *  last edit isn't dropped if the debounce window hasn't elapsed). */
function flushPendingSave() {
  if (saveTimer !== null) {
    clearTimeout(saveTimer);
    saveTimer = null;
    void save();
  }
}

// Watch every config field; deep: true because threshold values are nested
// inside the `config` ref. The `configLoaded` guard suppresses the assignment
// from `loadConfig`, which would otherwise re-save the just-loaded data.
watch(
  config,
  () => {
    if (!configLoaded.value) return;
    scheduleAutoSave();
  },
  { deep: true },
);

/** Apply a number-input change to a specific config field. v-text-field with type=number
 *  emits either a number or an empty string, so we normalize before assigning. */
function setNumericField(key: NumericConfigKey, value: number | string) {
  if (!config.value) return;
  const n = typeof value === "number" ? value : Number(value);
  if (Number.isFinite(n)) {
    (config.value as unknown as Record<NumericConfigKey, number>)[key] = n;
  }
}

function startPolling() {
  stopPolling();
  poll();
  pollTimer = setInterval(poll, 2000);
}

function stopPolling() {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  // Flush any pending auto-save so the user's last edit isn't lost on close.
  flushPendingSave();
}

watch(
  () => props.modelValue,
  (open) => {
    if (open) {
      // Reset the loaded-flag for the new session so the loadConfig assignment
      // doesn't re-trigger a save.
      configLoaded.value = false;
      loadConfig();
      startPolling();
    } else {
      stopPolling();
      errorMsg.value = null;
    }
  },
);

onMounted(() => {
  if (props.modelValue) {
    configLoaded.value = false;
    loadConfig();
    startPolling();
  }
});

onUnmounted(stopPolling);
</script>

<template>
  <v-dialog
    :model-value="modelValue"
    @update:model-value="emit('update:modelValue', $event)"
    max-width="800"
  >
    <v-card>
      <v-card-title class="d-flex align-center pa-3 pb-2">
        <v-icon icon="mdi-tune-vertical" class="mr-2" color="primary" />
        <span class="text-h6">{{ t('pbTitle') }}</span>
        <!-- Engine state chip lives next to the title (not in its own card row) -->
        <v-chip
          :color="engineStateColor"
          size="x-small"
          variant="flat"
          class="ml-3 font-weight-medium"
        >
          <v-icon start size="x-small" :icon="engineState === 'engaged' ? 'mdi-fire' : 'mdi-circle-small'" />
          {{ engineStateLabel }}
        </v-chip>
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" density="compact" @click="emit('update:modelValue', false)" />
      </v-card-title>
      <div class="px-4 pb-2 text-body-2 text-medium-emphasis">{{ t('pbSubtitle') }}</div>
      <!-- Live status line — only shown when there's actual data to convey
           (downgraded count, foreground info, or game-mode active). When the
           engine is idle/disabled/offline this whole row collapses out. -->
      <div
        v-if="showStatusDetails"
        class="px-4 pb-2 text-caption text-medium-emphasis d-flex align-center ga-3 flex-wrap"
      >
        <span v-if="status?.engaged && status.downgraded > 0">
          {{ t('pbDowngradedCount', { count: status.downgraded }) }}
        </span>
        <span v-if="serviceOnline && status?.fg_pid">
          {{ t('pbFgInfo', { pid: status.fg_pid, cpu: (status.fg_cpu_percent ?? 0).toFixed(0) }) }}
        </span>
        <v-spacer />
        <v-chip v-if="status?.game_mode_active" color="primary" size="x-small" variant="tonal">
          <v-icon start icon="mdi-gamepad-variant" size="x-small" />
          {{ t('pbGameModeActive') }}
        </v-chip>
      </div>

      <v-divider />

      <v-card-text class="pa-4" style="max-height: 72vh; overflow-y: auto">
        <!-- Error banner only — auto-save is silent on success -->
        <v-alert v-if="errorMsg" type="error" density="compact" variant="tonal" class="mb-3"
          closable @click:close="errorMsg = null">{{ errorMsg }}</v-alert>

        <template v-if="config">
          <v-progress-linear v-if="loading" indeterminate color="primary" class="mb-3" />

          <!-- Master toggles -->
          <v-switch v-model="config.enabled" :label="t('pbEnable')" color="primary"
            hide-details density="compact" class="mb-1" />
          <v-switch v-model="config.game_mode_enabled" color="primary"
            hide-details density="compact" class="mb-3">
            <template #label>
              <span>{{ t('pbGameMode') }}</span>
              <v-tooltip :text="t('pbGameModeHint')" location="top">
                <template #activator="{ props }">
                  <v-icon v-bind="props" icon="mdi-help-circle-outline" size="x-small"
                    color="grey-darken-1" class="ml-1" />
                </template>
              </v-tooltip>
            </template>
          </v-switch>

          <v-divider class="mb-3" />

          <!-- Triggers -->
          <div class="section-title text-subtitle-2 text-medium-emphasis mb-2">
            {{ t('pbSectionTriggers') }}
          </div>
          <v-row dense>
            <v-col v-for="f in thresholdFields" :key="f.key" cols="12" sm="6">
              <v-text-field
                :model-value="config[f.key]"
                @update:model-value="(v) => setNumericField(f.key, v)"
                :label="t(f.labelKey)"
                :suffix="f.suffix"
                type="number"
                :min="f.min" :max="f.max"
                density="compact" variant="outlined" hide-details
              >
                <template #append-inner>
                  <v-tooltip :text="t(f.hintKey)" location="top">
                    <template #activator="{ props }">
                      <v-icon v-bind="props" icon="mdi-help-circle-outline" size="small"
                        color="grey-darken-1" />
                    </template>
                  </v-tooltip>
                </template>
              </v-text-field>
            </v-col>
          </v-row>

          <!-- Timing -->
          <div class="section-title text-subtitle-2 text-medium-emphasis mb-2 mt-3">
            {{ t('pbSectionTiming') }}
          </div>
          <v-row dense>
            <v-col v-for="f in timingFields" :key="f.key" cols="12" sm="4">
              <v-text-field
                :model-value="config[f.key]"
                @update:model-value="(v) => setNumericField(f.key, v)"
                :label="t(f.labelKey)"
                :suffix="f.suffix"
                type="number"
                :min="f.min" :max="f.max"
                density="compact" variant="outlined" hide-details
              >
                <template #append-inner>
                  <v-tooltip :text="t(f.hintKey)" location="top">
                    <template #activator="{ props }">
                      <v-icon v-bind="props" icon="mdi-help-circle-outline" size="small"
                        color="grey-darken-1" />
                    </template>
                  </v-tooltip>
                </template>
              </v-text-field>
            </v-col>
          </v-row>

          <!-- Allowlist -->
          <div class="section-title text-subtitle-2 text-medium-emphasis mb-2 mt-3">
            {{ t('pbSectionAllowlist') }}
          </div>
          <v-combobox v-model="config.whitelist"
            :placeholder="t('pbWhitelistPlaceholder')"
            multiple chips closable-chips deletable-chips
            density="compact" variant="outlined" hide-details
          >
            <template #prepend-inner>
              <v-tooltip :text="t('pbWhitelistHint')" location="top">
                <template #activator="{ props }">
                  <v-icon v-bind="props" icon="mdi-help-circle-outline" size="small"
                    color="grey-darken-1" />
                </template>
              </v-tooltip>
            </template>
          </v-combobox>
        </template>

        <v-divider class="my-4" />

        <!-- Activity: stats inline with the section title, log table below -->
        <div class="d-flex align-center flex-wrap ga-2 mb-2">
          <span class="text-subtitle-2">{{ t('pbLogTitle') }}</span>
          <template v-if="statistics">
            <v-chip size="x-small" variant="tonal" color="warning">
              {{ t('pbStatsDowngrades', { count: statistics.downgrade_count }) }}
            </v-chip>
            <v-chip size="x-small" variant="tonal" color="success">
              {{ t('pbStatsRestores', { count: statistics.restore_count }) }}
            </v-chip>
            <v-chip size="x-small" variant="tonal">
              {{ t('pbStatsProcesses', { count: statistics.unique_processes }) }}
            </v-chip>
          </template>
        </div>
        <v-table v-if="logs.length > 0" density="compact" class="log-table">
          <thead>
            <tr>
              <th class="pl-0">{{ t('pbColTime') }}</th>
              <th>{{ t('pbColAction') }}</th>
              <th>{{ t('pbColProcess') }}</th>
              <th>{{ t('cpu') }}</th>
              <th>{{ t('priority') }}</th>
              <th class="pr-0">{{ t('pbColReason') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(log, i) in logs" :key="`${log.ts}-${log.pid}-${i}`">
              <td class="pl-0 text-no-wrap">{{ formatTime(log.ts) }}</td>
              <td>
                <v-chip :color="actionColor(log.action)" size="x-small" variant="flat">
                  {{ actionLabel(log.action) }}
                </v-chip>
              </td>
              <td class="text-no-wrap">{{ log.name }} ({{ log.pid }})</td>
              <td>{{ log.cpu !== null ? `${log.cpu.toFixed(0)}%` : '-' }}</td>
              <td class="text-no-wrap">
                <v-tooltip
                  :text="priorityFull(log)"
                  location="top"
                  :disabled="priorityShort(log) === '-'"
                >
                  <template #activator="{ props }">
                    <span v-bind="props" class="text-caption">{{ priorityShort(log) }}</span>
                  </template>
                </v-tooltip>
              </td>
              <td class="pr-0 text-caption">{{ reasonLabel(log.reason) }}</td>
            </tr>
          </tbody>
        </v-table>
        <div v-else class="text-body-2 text-medium-emphasis text-center py-4">
          {{ t('pbLogEmpty') }}
        </div>
      </v-card-text>
    </v-card>
  </v-dialog>
</template>

<style scoped>
.log-table :deep(th),
.log-table :deep(td) {
  white-space: nowrap;
}
.section-title {
  letter-spacing: 0.02em;
}
</style>
