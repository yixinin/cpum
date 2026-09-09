<script setup lang="ts">
import { ref, onMounted, watch } from "vue";
import type { AffinityRule } from "../api";
import {
  loadAffinityRules,
  addAffinityRule,
  updateAffinityRule,
  deleteAffinityRule,
  applyAffinityRules,
} from "../api";
import { parseMask, formatMask } from "../types";

const props = defineProps<{
  modelValue: boolean;
  topology: { total_logical_processors: number } | null;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: boolean];
  applied: [count: number];
}>();

// 规则列表
const rules = ref<AffinityRule[]>([]);
// 编辑中的规则
const editingRule = ref<AffinityRule | null>(null);
const isEditing = ref(false);
const isNewRule = ref(false);

// 应用状态
const applying = ref(false);
const errorMsg = ref<string | null>(null);
const successMsg = ref<string | null>(null);

// 加载规则
async function loadRules() {
  try {
    rules.value = await loadAffinityRules();
  } catch (e) {
    errorMsg.value = `加载规则失败: ${e}`;
  }
}


// 添加新规则
function addRule() {
  isNewRule.value = true;
  editingRule.value = {
    id: "",
    process_name: "",
    mask: props.topology ? formatMask((1n << BigInt(props.topology.total_logical_processors)) - 1n) : "0xFF",
    enabled: true,
    created_at: Date.now() / 1000,
    note: "",
  };
  isEditing.value = true;
}

// 编辑规则
function editRule(rule: AffinityRule) {
  isNewRule.value = false;
  editingRule.value = { ...rule };
  isEditing.value = true;
}

// 保存编辑
async function saveEdit() {
  if (!editingRule.value) return;
  
  errorMsg.value = null;
  
  // 验证
  if (!editingRule.value.process_name.trim()) {
    errorMsg.value = "请输入进程名";
    return;
  }
  
  try {
    // 验证mask格式
    parseMask(editingRule.value.mask);
  } catch {
    errorMsg.value = "无效的亲和性掩码格式";
    return;
  }
  
  try {
    if (isNewRule.value) {
      const newRule = await addAffinityRule(
        editingRule.value.process_name.trim(),
        editingRule.value.mask,
        editingRule.value.note
      );
      rules.value.push(newRule);
    } else {
      const updated = await updateAffinityRule(editingRule.value.id, {
        processName: editingRule.value.process_name.trim(),
        mask: editingRule.value.mask,
        enabled: editingRule.value.enabled,
        note: editingRule.value.note,
      });
      const idx = rules.value.findIndex(r => r.id === updated.id);
      if (idx >= 0) {
        rules.value[idx] = updated;
      }
    }
    
    isEditing.value = false;
    editingRule.value = null;
    successMsg.value = isNewRule.value ? "规则已添加" : "规则已更新";
    setTimeout(() => successMsg.value = null, 2000);
  } catch (e) {
    errorMsg.value = `保存规则失败: ${e}`;
  }
}

// 取消编辑
function cancelEdit() {
  isEditing.value = false;
  editingRule.value = null;
  errorMsg.value = null;
}

// 删除规则
async function removeRule(id: string) {
  if (!confirm("确定要删除这条规则吗？")) return;
  
  try {
    await deleteAffinityRule(id);
    rules.value = rules.value.filter(r => r.id !== id);
    successMsg.value = "规则已删除";
    setTimeout(() => successMsg.value = null, 2000);
  } catch (e) {
    errorMsg.value = `删除规则失败: ${e}`;
  }
}

// 切换规则启用状态
async function toggleRule(rule: AffinityRule) {
  try {
    const updated = await updateAffinityRule(rule.id, {
      enabled: !rule.enabled,
    });
    const idx = rules.value.findIndex(r => r.id === updated.id);
    if (idx >= 0) {
      rules.value[idx] = updated;
    }
  } catch (e) {
    errorMsg.value = `更新规则失败: ${e}`;
  }
}

// 应用所有规则
async function applyRules() {
  applying.value = true;
  errorMsg.value = null;
  
  try {
    const count = await applyAffinityRules();
    emit("applied", count);
    successMsg.value = `已成功应用到 ${count} 个进程`;
    setTimeout(() => successMsg.value = null, 3000);
  } catch (e) {
    errorMsg.value = `应用规则失败: ${e}`;
  } finally {
    applying.value = false;
  }
}

// 格式化时间
function formatTime(timestamp: number): string {
  return new Date(timestamp * 1000).toLocaleString();
}

// 掩码预览
function maskPreview(mask: string): string {
  try {
    const parsed = parseMask(mask);
    const count = popcount(parsed);
    return `${mask} (${count}核)`;
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

// 监听对话框打开
watch(() => props.modelValue, (val) => {
  if (val) {
    loadRules();
  }
});

onMounted(() => {
  if (props.modelValue) {
    loadRules();
  }
});
</script>

<template>
  <!-- 主规则管理对话框 -->
  <v-dialog :model-value="modelValue" @update:model-value="emit('update:modelValue', $event)" max-width="800">
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-ruler" class="mr-2" />
        亲和性规则管理
        <v-spacer />
        <v-btn icon="mdi-close" variant="text" @click="emit('update:modelValue', false)" />
      </v-card-title>
      
      <v-card-text>
        <!-- 提示信息 -->
        <v-alert v-if="errorMsg" type="error" density="compact" class="mb-3" closable @click:close="errorMsg = null">
          {{ errorMsg }}
        </v-alert>
        <v-alert v-if="successMsg" type="success" density="compact" class="mb-3">
          {{ successMsg }}
        </v-alert>
        
        <!-- 操作按钮 -->
        <div class="d-flex gap-2 mb-4">
          <v-btn color="primary" prepend-icon="mdi-plus" @click="addRule">
            添加规则
          </v-btn>
          <v-btn 
            color="success" 
            prepend-icon="mdi-play" 
            :loading="applying"
            :disabled="rules.length === 0"
            @click="applyRules"
          >
            应用规则
          </v-btn>
          <v-spacer />
          <v-chip v-if="rules.length > 0" color="info" variant="outlined">
            {{ rules.filter(r => r.enabled).length }} / {{ rules.length }} 条规则已启用
          </v-chip>
        </div>
        
        <!-- 规则列表 -->
        <v-table v-if="rules.length > 0">
          <thead>
            <tr>
              <th>启用</th>
              <th>进程名</th>
              <th>亲和性掩码</th>
              <th>备注</th>
              <th>创建时间</th>
              <th>操作</th>
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
              </td>
              <td>
                <v-chip size="small" variant="outlined">
                  {{ maskPreview(rule.mask) }}
                </v-chip>
              </td>
              <td>{{ rule.note || "-" }}</td>
              <td class="text-caption">{{ formatTime(rule.created_at) }}</td>
              <td>
                <v-btn icon="mdi-pencil" size="small" variant="text" @click="editRule(rule)" />
                <v-btn icon="mdi-delete" size="small" variant="text" color="error" @click="removeRule(rule.id)" />
              </td>
            </tr>
          </tbody>
        </v-table>
        
        <!-- 空状态 -->
        <v-alert v-else type="info" variant="tonal" class="mt-4">
          暂无亲和性规则。点击"添加规则"创建第一条规则。
          <br>
          <small>规则会自动保存，下次启动时自动加载。</small>
        </v-alert>
        
        <!-- 说明 -->
        <v-expansion-panels class="mt-4">
          <v-expansion-panel>
            <v-expansion-panel-title>
              <v-icon icon="mdi-information" class="mr-2" />
              使用说明
            </v-expansion-panel-title>
            <v-expansion-panel-text>
              <ul class="text-body-2">
                <li><strong>进程名</strong>：不区分大小写，可以带或不带 .exe 后缀</li>
                <li><strong>亲和性掩码</strong>：十六进制格式，如 0xFF 表示 CPU 0-7</li>
                <li><strong>应用规则</strong>：将规则应用到当前运行的所有匹配进程</li>
                <li><strong>自动应用</strong>：规则保存后，新启动的进程不会自动应用，需要手动点击"应用规则"</li>
              </ul>
            </v-expansion-panel-text>
          </v-expansion-panel>
        </v-expansion-panels>
      </v-card-text>
      
      <v-card-actions>
        <v-spacer />
        <v-btn @click="emit('update:modelValue', false)">关闭</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
  
  <!-- 编辑对话框 (独立于主对话框) -->
  <v-dialog v-model="isEditing" max-width="500">
    <v-card>
      <v-card-title>{{ isNewRule ? "添加规则" : "编辑规则" }}</v-card-title>
      <v-card-text>
        <v-text-field
          v-if="editingRule"
          v-model="editingRule.process_name"
          label="进程名"
          placeholder="例如: TRAE SOLO CN"
          hint="不区分大小写，可以带或不带 .exe"
          persistent-hint
          class="mb-3"
        />
        
        <v-text-field
          v-if="editingRule"
          v-model="editingRule.mask"
          label="亲和性掩码"
          placeholder="0xFF"
          hint="十六进制格式，如 0xFF (CPU 0-7), 0x0F (CPU 0-3)"
          persistent-hint
          class="mb-3"
        />
        
        <v-text-field
          v-if="editingRule"
          v-model="editingRule.note"
          label="备注（可选）"
          placeholder="例如：限制在CCD0"
          class="mb-3"
        />
        
        <v-alert v-if="errorMsg" type="error" density="compact" class="mt-3">
          {{ errorMsg }}
        </v-alert>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn @click="cancelEdit">取消</v-btn>
        <v-btn color="primary" @click="saveEdit">
          {{ isNewRule ? "添加" : "保存" }}
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
