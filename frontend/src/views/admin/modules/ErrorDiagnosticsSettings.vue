<template>
  <PageContainer>
    <PageHeader
      title="错误诊断"
      description="配置 AI 摘要器，自动总结上游 4xx / 回空等错误；诊断记录见错误诊断页"
    />

    <div class="mt-6 space-y-6">
      <CardSection
        title="AI 摘要器"
        description="配置一个 OpenAI 兼容服务，用于在诊断详情中一键生成错误的自然语言摘要"
      >
        <template #actions>
          <Button
            size="sm"
            :disabled="saving"
            @click="saveConfig"
          >
            {{ saving ? '保存中...' : '保存' }}
          </Button>
        </template>

        <div class="space-y-5">
          <div class="flex items-center justify-between gap-4 rounded-lg border border-border/70 px-4 py-3">
            <div>
              <Label class="text-sm font-medium">
                摘要器状态
              </Label>
              <p class="mt-1 text-xs text-muted-foreground">
                {{ configured ? '已配置，可在诊断详情中生成 AI 摘要' : '未配置（保存后生效）' }}
              </p>
            </div>
            <Badge :variant="configured ? 'success' : 'secondary'">
              {{ configured ? '已启用' : '未启用' }}
            </Badge>
          </div>

          <div>
            <Label
              for="summarizer-base-url"
              class="block text-sm font-medium"
            >
              Base URL
            </Label>
            <Input
              id="summarizer-base-url"
              v-model="baseUrlInput"
              placeholder="https://api.openai.com/v1"
              class="mt-1"
            />
            <p class="mt-1 text-xs text-muted-foreground">
              OpenAI 兼容服务的根地址（不含 /chat/completions）
            </p>
          </div>

          <div>
            <Label
              for="summarizer-api-key"
              class="block text-sm font-medium"
            >
              API Key
            </Label>
            <Input
              id="summarizer-api-key"
              v-model="apiKeyInput"
              masked
              :placeholder="apiKeyIsSet ? '已设置（留空保持不变）' : 'sk-...'"
              class="mt-1"
            />
          </div>

          <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div>
              <Label
                for="summarizer-model"
                class="block text-sm font-medium"
              >
                模型
              </Label>
              <Input
                id="summarizer-model"
                v-model="modelInput"
                placeholder="gpt-4o-mini"
                class="mt-1"
              />
            </div>
            <div>
              <Label
                for="summarizer-timeout"
                class="block text-sm font-medium"
              >
                超时（秒）
              </Label>
              <Input
                id="summarizer-timeout"
                v-model.number="timeoutInput"
                type="number"
                min="1"
                class="mt-1"
              />
            </div>
          </div>
        </div>
      </CardSection>

      <CardSection
        title="诊断入口"
        description="查看上游回空 / 4xx / 5xx 的完整取证记录，支持按会话检索与 AI 摘要"
      >
        <RouterLink
          to="/admin/diagnostics"
          class="inline-flex h-11 items-center rounded-xl px-3 text-sm text-primary hover:underline"
        >
          打开错误诊断
        </RouterLink>
      </CardSection>
    </div>
  </PageContainer>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { RouterLink } from 'vue-router'
import { Badge, Button, Input, Label } from '@/components/ui'
import { PageHeader, PageContainer, CardSection } from '@/components/layout'
import { adminApi } from '@/api/admin'
import { useToast } from '@/composables/useToast'
import { parseApiError } from '@/utils/errorParser'
import { log } from '@/utils/logger'

const CONFIG_KEY = 'error_diagnostic_summarizer'

const { success, error } = useToast()

const saving = ref(false)
const configured = ref(false)
const apiKeyIsSet = ref(false)
const baseUrlInput = ref('')
const apiKeyInput = ref('')
const modelInput = ref('')
const timeoutInput = ref(30)

const canSave = computed(() => {
  return baseUrlInput.value.trim() !== '' && modelInput.value.trim() !== '' && (
    apiKeyIsSet.value || apiKeyInput.value.trim() !== ''
  )
})

onMounted(() => {
  void loadConfig()
})

async function loadConfig() {
  try {
    const detail = await adminApi.getSystemConfig(CONFIG_KEY, { cacheTtlMs: 0 })
    const value = (detail.value ?? null) as null | {
      base_url?: string
      api_key?: string | null
      model?: string
      timeout_secs?: number
    }
    baseUrlInput.value = typeof value?.base_url === 'string' ? value.base_url : ''
    modelInput.value = typeof value?.model === 'string' ? value.model : ''
    timeoutInput.value = typeof value?.timeout_secs === 'number' ? value.timeout_secs : 30
    apiKeyInput.value = ''
    // The backend masks api_key; is_set reflects whether a key is stored.
    apiKeyIsSet.value = detail.is_set === true
    configured.value = baseUrlInput.value !== '' && modelInput.value !== '' && apiKeyIsSet.value
  } catch (err) {
    if (!isNotFoundError(err)) {
      error(parseApiError(err, '加载 AI 摘要器配置失败'))
      log.error('加载 AI 摘要器配置失败:', err)
    }
  }
}

async function saveConfig() {
  if (!canSave.value) {
    error('请填写 Base URL、模型与 API Key')
    return
  }
  saving.value = true
  try {
    const payload: Record<string, unknown> = {
      base_url: baseUrlInput.value.trim(),
      model: modelInput.value.trim(),
      timeout_secs: Number.isFinite(timeoutInput.value) && timeoutInput.value > 0
        ? Math.floor(timeoutInput.value)
        : 30,
      // Empty api_key tells the backend to keep the previously stored key.
      api_key: apiKeyInput.value.trim(),
    }
    await adminApi.updateSystemConfig(CONFIG_KEY, payload, '错误诊断 AI 摘要器配置')
    if (apiKeyInput.value.trim() !== '') {
      apiKeyIsSet.value = true
      apiKeyInput.value = ''
    }
    configured.value = true
    success('AI 摘要器配置已保存')
  } catch (err) {
    error(parseApiError(err, '保存 AI 摘要器配置失败'))
    log.error('保存 AI 摘要器配置失败:', err)
  } finally {
    saving.value = false
  }
}

function isNotFoundError(err: unknown): boolean {
  const status = (err as { response?: { status?: number } })?.response?.status
  return status === 404
}
</script>
