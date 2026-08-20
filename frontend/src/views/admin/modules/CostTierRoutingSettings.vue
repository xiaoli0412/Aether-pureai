<template>
  <PageContainer>
    <PageHeader
      title="成本分层路由"
      description="上游渠道（提供商/Key）在此标记按量或按次计费；下游请求按上下文大小自动路由——小上下文优先按量渠道，大上下文优先按次渠道，利润优先，兼顾会话与缓存粘性"
    />

    <div class="mt-6 space-y-6">
      <!-- 全局路由策略 -->
      <CardSection
        title="全局路由策略"
        description="对所有模型生效：根据请求上下文 Token 数与阈值的大小关系，选择目标计费类型并重排候选渠道"
      >
        <template #actions>
          <Button
            size="sm"
            :disabled="savingPolicy"
            @click="savePolicy"
          >
            {{ savingPolicy ? '保存中...' : '保存' }}
          </Button>
        </template>

        <div class="space-y-5">
          <div class="flex items-center justify-between gap-4 rounded-lg border border-border/70 px-4 py-3">
            <div>
              <Label class="text-sm font-medium">
                启用成本分层路由
              </Label>
              <p class="mt-1 text-xs text-muted-foreground">
                关闭后完全恢复默认路由顺序，不产生任何影响
              </p>
            </div>
            <Switch v-model="policyEnabled" />
          </div>

          <div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div>
              <Label
                for="cost-threshold"
                class="block text-sm font-medium"
              >
                上下文阈值（Tokens）
              </Label>
              <Input
                id="cost-threshold"
                v-model.number="threshold"
                type="number"
                min="1"
                placeholder="例如 20000"
                class="mt-1"
              />
              <p class="mt-1 text-xs text-muted-foreground">
                低于阈值走"低于阈值偏好"，高于阈值走"高于阈值偏好"
              </p>
            </div>
            <div>
              <Label class="block text-sm font-medium">
                低于阈值 → 优先
              </Label>
              <Select
                v-model="belowPrefer"
                class="mt-1"
              >
                <SelectTrigger>
                  <SelectValue placeholder="选择计费类型" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="per_use">
                    按量计费渠道
                  </SelectItem>
                  <SelectItem value="per_request">
                    按次计费渠道
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div>
              <Label class="block text-sm font-medium">
                高于阈值 → 优先
              </Label>
              <Select
                v-model="abovePrefer"
                class="mt-1"
              >
                <SelectTrigger>
                  <SelectValue placeholder="选择计费类型" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="per_request">
                    按次计费渠道
                  </SelectItem>
                  <SelectItem value="per_use">
                    按量计费渠道
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>

          <div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div class="flex items-center justify-between gap-3 rounded-lg border border-border/70 px-4 py-3 sm:col-span-1">
              <div>
                <Label class="text-sm font-medium">
                  会话粘性优先
                </Label>
                <p class="mt-1 text-xs text-muted-foreground">
                  尊重会话亲和渠道
                </p>
              </div>
              <Switch v-model="respectSession" />
            </div>
            <div class="flex items-center justify-between gap-3 rounded-lg border border-border/70 px-4 py-3 sm:col-span-1">
              <div>
                <Label class="text-sm font-medium">
                  缓存粘性优先
                </Label>
                <p class="mt-1 text-xs text-muted-foreground">
                  尊重缓存亲和渠道
                </p>
              </div>
              <Switch v-model="respectCache" />
            </div>
            <div>
              <Label
                for="cost-sacrifice"
                class="block text-sm font-medium"
              >
                最大利润让步（USD）
              </Label>
              <Input
                id="cost-sacrifice"
                v-model.number="maxSacrifice"
                type="number"
                min="0"
                step="0.001"
                class="mt-1"
              />
              <p class="mt-1 text-xs text-muted-foreground">
                粘性渠道与最优渠道的利润差在该范围内才保留粘性；0 = 利润绝对优先
              </p>
            </div>
          </div>
        </div>
      </CardSection>

      <!-- 渠道计费分类 -->
      <CardSection
        title="渠道计费分类（上游）"
        description="为每个渠道商（提供商）标记按量/按次计费；展开可对单个 Key（K）单独覆盖。未标记的渠道按价格数据自动推断"
      >
        <template #actions>
          <Button
            size="sm"
            variant="outline"
            :disabled="loadingProviders"
            @click="loadProviders"
          >
            {{ loadingProviders ? '刷新中...' : '刷新' }}
          </Button>
        </template>

        <div
          v-if="loadingProviders && providers.length === 0"
          class="rounded-md border border-border px-3 py-4 text-center text-sm text-muted-foreground"
        >
          加载中...
        </div>
        <div
          v-else-if="providers.length === 0"
          class="rounded-md border border-border px-3 py-4 text-center text-sm text-muted-foreground"
        >
          暂无渠道（提供商）
        </div>
        <div
          v-else
          class="space-y-2"
        >
          <div class="mb-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            <Badge variant="secondary">
              按量 {{ countByClass('per_use') }}
            </Badge>
            <Badge variant="secondary">
              按次 {{ countByClass('per_request') }}
            </Badge>
            <Badge variant="outline">
              未标记 {{ countByClass('') }}
            </Badge>
            <span class="ml-2">提示：只有同一模型同时存在两类渠道时，分层路由才会对该模型产生切换</span>
          </div>

          <div
            v-for="provider in providers"
            :key="provider.id"
            class="rounded-lg border border-border/70"
          >
            <div class="flex flex-col gap-2 px-4 py-3 sm:flex-row sm:items-center sm:justify-between">
              <div class="flex min-w-0 items-center gap-2">
                <ChevronRight
                  class="h-4 w-4 shrink-0 cursor-pointer text-muted-foreground transition-transform"
                  :class="{ 'rotate-90': expanded[provider.id] }"
                  @click="toggleExpand(provider.id)"
                />
                <span class="truncate text-sm font-medium">{{ provider.name }}</span>
                <Badge
                  v-if="!provider.is_active"
                  variant="secondary"
                >
                  停用
                </Badge>
              </div>
              <div class="flex items-center gap-2">
                <Select
                  :model-value="providerBillingClass(provider)"
                  @update:model-value="(value: string) => saveProviderClass(provider, value)"
                >
                  <SelectTrigger class="h-9 w-40">
                    <SelectValue placeholder="未标记" />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="">
                      未标记（自动推断）
                    </SelectItem>
                    <SelectItem value="per_use">
                      按量计费
                    </SelectItem>
                    <SelectItem value="per_request">
                      按次计费
                    </SelectItem>
                  </SelectContent>
                </Select>
                <Loader2
                  v-if="savingProvider === provider.id"
                  class="h-4 w-4 animate-spin text-muted-foreground"
                />
              </div>
            </div>

            <!-- Key 级覆盖 -->
            <div
              v-if="expanded[provider.id]"
              class="border-t border-border/50 px-4 py-3"
            >
              <div
                v-if="keysLoading[provider.id]"
                class="py-2 text-center text-xs text-muted-foreground"
              >
                加载 Key 列表...
              </div>
              <div
                v-else-if="!(providerKeys[provider.id] || []).length"
                class="py-2 text-center text-xs text-muted-foreground"
              >
                该渠道下没有 Key
              </div>
              <div
                v-else
                class="space-y-2"
              >
                <div
                  v-for="key in providerKeys[provider.id]"
                  :key="key.id"
                  class="flex flex-col gap-2 rounded-md bg-muted/30 px-3 py-2 sm:flex-row sm:items-center sm:justify-between"
                >
                  <div class="min-w-0">
                    <span class="block truncate text-sm">{{ key.name || key.id }}</span>
                    <span class="block truncate text-xs text-muted-foreground">
                      {{ keyBillingClass(key) ? '已单独标记' : '跟随渠道标记' }}
                    </span>
                  </div>
                  <div class="flex items-center gap-2">
                    <Select
                      :model-value="keyBillingClass(key)"
                      @update:model-value="(value: string) => saveKeyClass(provider, key, value)"
                    >
                      <SelectTrigger class="h-8 w-40">
                        <SelectValue placeholder="跟随渠道" />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="">
                          跟随渠道
                        </SelectItem>
                        <SelectItem value="per_use">
                          按量计费
                        </SelectItem>
                        <SelectItem value="per_request">
                          按次计费
                        </SelectItem>
                      </SelectContent>
                    </Select>
                    <Loader2
                      v-if="savingKey === key.id"
                      class="h-4 w-4 animate-spin text-muted-foreground"
                    />
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
      </CardSection>
    </div>
  </PageContainer>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import {
  Badge,
  Button,
  Input,
  Label,
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
  Switch
} from '@/components/ui'
import { PageHeader, PageContainer, CardSection } from '@/components/layout'
import { ChevronRight, Loader2 } from 'lucide-vue-next'
import { adminApi } from '@/api/admin'
import { getProvidersSummary, updateProvider } from '@/api/endpoints/providers'
import { getProviderKeys, updateProviderKey } from '@/api/endpoints/keys'
import type { ProviderWithEndpointsSummary, EndpointAPIKey } from '@/api/endpoints/types/provider'
import { useToast } from '@/composables/useToast'
import { parseApiError } from '@/utils/errorParser'
import { log } from '@/utils/logger'

const CONFIG_KEY = 'cost_tier_routing'

const { success, error } = useToast()

// 全局策略
const savingPolicy = ref(false)
const policyEnabled = ref(false)
const threshold = ref<number | null>(null)
const belowPrefer = ref('per_use')
const abovePrefer = ref('per_request')
const respectSession = ref(true)
const respectCache = ref(true)
const maxSacrifice = ref(0)

// 渠道分类
const loadingProviders = ref(false)
const providers = ref<ProviderWithEndpointsSummary[]>([])
const savingProvider = ref<string | null>(null)
const expanded = ref<Record<string, boolean>>({})
const providerKeys = ref<Record<string, EndpointAPIKey[]>>({})
const keysLoading = ref<Record<string, boolean>>({})
const savingKey = ref<string | null>(null)

onMounted(async () => {
  await Promise.all([loadPolicy(), loadProviders()])
})

async function loadPolicy() {
  try {
    const detail = await adminApi.getSystemConfig(CONFIG_KEY, { cacheTtlMs: 0 })
    const value = (detail.value ?? null) as null | {
      enabled?: boolean
      context_threshold_tokens?: number
      below_prefer?: string
      above_prefer?: string
      stickiness?: {
        respect_session_affinity?: boolean
        respect_cache_affinity?: boolean
        max_profit_sacrifice_usd?: number
      }
    }
    policyEnabled.value = value?.enabled === true
    threshold.value = typeof value?.context_threshold_tokens === 'number' && value.context_threshold_tokens > 0
      ? value.context_threshold_tokens
      : null
    belowPrefer.value = value?.below_prefer === 'per_request' ? 'per_request' : 'per_use'
    abovePrefer.value = value?.above_prefer === 'per_use' ? 'per_use' : 'per_request'
    respectSession.value = value?.stickiness?.respect_session_affinity !== false
    respectCache.value = value?.stickiness?.respect_cache_affinity !== false
    maxSacrifice.value = typeof value?.stickiness?.max_profit_sacrifice_usd === 'number'
      ? value.stickiness.max_profit_sacrifice_usd
      : 0
  } catch (err) {
    const status = (err as { response?: { status?: number } })?.response?.status
    if (status !== 404) {
      error(parseApiError(err, '加载成本路由策略失败'))
      log.error('加载成本路由策略失败:', err)
    }
  }
}

async function savePolicy() {
  savingPolicy.value = true
  try {
    const thresholdValue = Math.floor(Number(threshold.value))
    const payload: Record<string, unknown> = {
      enabled: policyEnabled.value,
      context_threshold_tokens: Number.isFinite(thresholdValue) && thresholdValue > 0
        ? thresholdValue
        : null,
      below_prefer: belowPrefer.value,
      above_prefer: abovePrefer.value,
      stickiness: {
        respect_session_affinity: respectSession.value,
        respect_cache_affinity: respectCache.value,
        max_profit_sacrifice_usd: Number.isFinite(maxSacrifice.value) && maxSacrifice.value >= 0
          ? maxSacrifice.value
          : 0,
      },
    }
    await adminApi.updateSystemConfig(CONFIG_KEY, payload, '成本分层路由策略')
    success('成本分层路由策略已保存')
  } catch (err) {
    error(parseApiError(err, '保存成本路由策略失败'))
    log.error('保存成本路由策略失败:', err)
  } finally {
    savingPolicy.value = false
  }
}

async function loadProviders() {
  loadingProviders.value = true
  try {
    const page = await getProvidersSummary({ page: 1, page_size: 200 }, { cacheTtlMs: 0 })
    providers.value = page.items || []
  } catch (err) {
    error(parseApiError(err, '加载渠道列表失败'))
    log.error('加载渠道列表失败:', err)
  } finally {
    loadingProviders.value = false
  }
}

function providerBillingClass(provider: ProviderWithEndpointsSummary): string {
  return provider.cost_billing_class || ''
}

function keyBillingClass(key: EndpointAPIKey): string {
  const value = key.capabilities?.cost_billing_class
  return typeof value === 'string' ? value : ''
}

function countByClass(cls: string): number {
  return providers.value.filter((provider) => providerBillingClass(provider) === cls).length
}

async function saveProviderClass(provider: ProviderWithEndpointsSummary, value: string) {
  savingProvider.value = provider.id
  try {
    await updateProvider(provider.id, {
      cost_billing_class: value === '' ? null : value,
    } as never)
    provider.cost_billing_class = value === '' ? null : value
    success(`渠道「${provider.name}」计费分类已更新`)
  } catch (err) {
    error(parseApiError(err, '更新渠道计费分类失败'))
    log.error('更新渠道计费分类失败:', err)
  } finally {
    savingProvider.value = null
  }
}

async function toggleExpand(providerId: string) {
  const willExpand = !expanded.value[providerId]
  expanded.value[providerId] = willExpand
  if (willExpand && !providerKeys.value[providerId]) {
    keysLoading.value[providerId] = true
    try {
      providerKeys.value[providerId] = await getProviderKeys(providerId)
    } catch (err) {
      error(parseApiError(err, '加载 Key 列表失败'))
      log.error('加载 Key 列表失败:', err)
    } finally {
      keysLoading.value[providerId] = false
    }
  }
}

async function saveKeyClass(
  provider: ProviderWithEndpointsSummary,
  key: EndpointAPIKey,
  value: string,
) {
  savingKey.value = key.id
  try {
    const capabilities: Record<string, unknown> = { ...(key.capabilities || {}) }
    if (value === '') {
      delete capabilities.cost_billing_class
    } else {
      capabilities.cost_billing_class = value
    }
    await updateProviderKey(key.id, { capabilities })
    key.capabilities = capabilities
    success(`Key「${key.name || key.id}」计费分类已更新`)
  } catch (err) {
    error(parseApiError(err, '更新 Key 计费分类失败'))
    log.error('更新 Key 计费分类失败:', err)
  } finally {
    savingKey.value = null
  }
}
</script>
