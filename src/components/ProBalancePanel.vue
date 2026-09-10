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
const saving = ref(false);
const errorMsg = ref<string | null>(null);
const successMsg = ref<string | null>(null);

/** Status polling handle (refreshes status + log every 2s while the panel is open) */
let pollTimer: ReturnType<typeof setInterval> | null = null;
/** If the status file is older than this many seconds, the service is considered offline */
const STATUS_FRESH_SECS = 5;

// ---------- Derived engine state ----------

/** Whether the service is online (based on status-file freshness) */
const serviceOnline = computed(() => {
  if (!status.value) return false;
  return Date.now() / 1000 - status.value.ts < STATUS_FRESH_SECS;
});

/** Engine runtime state: disabled / engaged / idle */
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

// ---------- Log display helpers ----------

function actionLabel(action: string): string {
  switch (action) {
    case "downgrade":
      return t("pbActionDowngrade");
    case "restore":
      return t("pbActionRestore");
    case "exit":
      return t("pbActionExit");
    case "game_mode_boost":
      return t("pbActionGameBoost");
    case "game_mode_restore":
      return t("pbActionGameRestore");
    default:
      return action;
  }
}

function actionColor(action: string): string {
  switch (action) {
    case "downgrade":
      return "warning";
    case "restore":
      return "success";
    default:
      return "grey";
  }
}

function reasonLabel(reason: string | null): string {
  switch (reason) {
    case "contention_cleared":
      return t("pbReasonContentionCleared");
    case "timeout":
      return t("pbReasonTimeout");
    case "process_exited":
      return t("pbReasonProcessExited");
    case "disabled":
      return t("pbReasonDisabled");
    case "shutdown":
      return t("pbReasonShutdown");
    case "fullscreen_ended":
      return t("pbReasonFullscreenEnded");
    case "foreground_changed":
      return t("pbReasonForegroundChanged");
    case "startup_reconcile":
      return t("pbReasonStartupReconcile");
    default:
      return "";
  }
}

/** Priority change summary: "Normal → Below Normal" (CPU tier shown, I/O appended) */
function prioritySummary(entry: PbLogEntry): string {
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
  saving.value = true;
  try {
    await saveProBalanceConfig(config.value);
    successMsg.value = t("pbSaved");
    errorMsg.value = null;
    setTimeout(() => (successMsg.value = null), 3000);
  } catch (e) {
    errorMsg.value = t("pbSaveFailed", { error: String(e) });
  } finally {
    saving.value = false;
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
}

watch(
  () => props.modelValue,
  (open) => {
    if (open) {
      loadConfig();
      startPolling();
    } else {
      stopPolling();
      successMsg.value = null;
      errorMsg.value = null;
    }
  },
);

onMounted(() => {
  if (props.modelValue) {
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
    max-width="860"
  >
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-tune-vertical" class="mr-2" color="primary" />
        {{ t('pbTitle') }}
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" @click="emit('update:modelValue', false)" />
      </v-card-title>

      <v-card-text style="max-height: 70vh; overflow-y: auto">
        <!-- Description -->
        <v-alert type="info" density="compact" variant="tonal" class="mb-4">
          {{ t('pbDesc') }}
        </v-alert>

        <v-alert v-if="errorMsg" type="error" density="compact" class="mb-3" closable @click:close="errorMsg = null">
          {{ errorMsg }}
        </v-alert>
        <v-alert v-if="successMsg" type="success" density="compact" class="mb-3">
          {{ successMsg }}
        </v-alert>

        <!-- Engine status -->
        <div class="d-flex align-center flex-wrap gap-2 mb-4">
          <span class="text-subtitle-2">{{ t('pbStatusTitle') }}:</span>
          <v-chip :color="engineStateColor" size="small" variant="flat">
            <v-icon start :icon="engineState === 'engaged' ? 'mdi-fire' : 'mdi-circle-small'" />
            {{ engineStateLabel }}
          </v-chip>
          <v-chip
            v-if="engineState === 'engaged' && status"
            color="warning"
            size="small"
            variant="outlined"
          >
            {{ t('pbDowngradedCount', { count: status.downgraded }) }}
          </v-chip>
          <v-chip
            v-if="engineState !== 'offline' && status?.fg_pid"
            size="small"
            variant="outlined"
          >
            {{ t('pbFgInfo', { pid: status.fg_pid, cpu: (status.fg_cpu_percent ?? 0).toFixed(0) }) }}
          </v-chip>
          <v-chip v-if="status?.game_mode_active" color="primary" size="small" variant="outlined">
            {{ t('pbGameModeActive') }}
          </v-chip>
          <v-chip v-if="!status" size="small" variant="outlined" color="grey">
            {{ t('pbNoStatus') }}
          </v-chip>
        </div>

        <v-divider class="mb-4" />

        <!-- Config editor -->
        <template v-if="config">
          <v-progress-linear v-if="loading" indeterminate color="primary" class="mb-2" />

          <!-- Master switch -->
          <v-switch
            v-model="config.enabled"
            :label="t('pbEnable')"
            color="primary"
            hide-details
            class="mb-2"
          />

          <v-switch
            v-model="config.game_mode_enabled"
            :label="t('pbGameMode')"
            :hint="t('pbGameModeHint')"
            persistent-hint
            color="primary"
            class="mb-2"
          />

          <!-- Threshold parameters -->
          <v-row dense>
            <v-col cols="12" sm="6">
              <v-text-field
                v-model.number="config.fg_cpu_threshold"
                :label="t('pbFgThreshold')"
                :hint="t('pbFgThresholdHint')"
                persistent-hint
                type="number"
                min="10"
                max="10000"
                density="compact"
              />
            </v-col>
            <v-col cols="12" sm="6">
              <v-text-field
                v-model.number="config.bg_cpu_threshold"
                :label="t('pbBgThreshold')"
                :hint="t('pbBgThresholdHint')"
                persistent-hint
                type="number"
                min="1"
                max="10000"
                density="compact"
              />
            </v-col>
            <v-col cols="12" sm="4">
              <v-text-field
                v-model.number="config.sustain_secs"
                :label="t('pbSustain')"
                :hint="t('pbSustainHint')"
                persistent-hint
                type="number"
                min="1"
                max="600"
                density="compact"
              />
            </v-col>
            <v-col cols="12" sm="4">
              <v-text-field
                v-model.number="config.restore_after_secs"
                :label="t('pbRestoreAfter')"
                :hint="t('pbRestoreAfterHint')"
                persistent-hint
                type="number"
                min="1"
                max="3600"
                density="compact"
              />
            </v-col>
            <v-col cols="12" sm="4">
              <v-text-field
                v-model.number="config.max_downgrade_secs"
                :label="t('pbMaxDowngrade')"
                :hint="t('pbMaxDowngradeHint')"
                persistent-hint
                type="number"
                min="30"
                max="86400"
                density="compact"
              />
            </v-col>
          </v-row>

          <!-- Allowlist -->
          <v-combobox
            v-model="config.whitelist"
            :label="t('pbWhitelist')"
            :hint="t('pbWhitelistHint')"
            persistent-hint
            :placeholder="t('pbWhitelistPlaceholder')"
            multiple
            chips
            closable-chips
            deletable-chips
            density="compact"
            class="mt-2"
          />

          <!-- Save -->
          <div class="d-flex justify-end mt-2 mb-4">
            <v-btn color="primary" prepend-icon="mdi-content-save" :loading="saving" @click="save">
              {{ t('save') }}
            </v-btn>
          </div>
        </template>

        <v-divider class="mb-3" />

        <div v-if="statistics" class="d-flex flex-wrap ga-2 mb-3">
          <v-chip size="small" variant="tonal">{{ t('pbStatsDowngrades', { count: statistics.downgrade_count }) }}</v-chip>
          <v-chip size="small" variant="tonal">{{ t('pbStatsRestores', { count: statistics.restore_count }) }}</v-chip>
          <v-chip size="small" variant="tonal">{{ t('pbStatsProcesses', { count: statistics.unique_processes }) }}</v-chip>
        </div>

        <!-- Action log -->
        <div class="text-subtitle-2 mb-2">{{ t('pbLogTitle') }}</div>
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
              <td class="text-no-wrap" style="max-width: 260px; overflow: hidden; text-overflow: ellipsis">
                {{ prioritySummary(log) }}
              </td>
              <td class="pr-0">{{ reasonLabel(log.reason) }}</td>
            </tr>
          </tbody>
        </v-table>
        <div v-else class="text-body-2 text-medium-emphasis">{{ t('pbLogEmpty') }}</div>
      </v-card-text>
    </v-card>
  </v-dialog>
</template>

<style scoped>
.log-table :deep(th),
.log-table :deep(td) {
  white-space: nowrap;
}
</style>
