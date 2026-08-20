<template>
  <PageContainer>
    <PageHeader
      title="回空屏蔽"
      description="当上游对同一会话 / 相同请求持续返回空响应时，本地临时屏蔽该来源，直接回安全审查响应，避免浪费上游配额"
    />

    <div class="mt-6 space-y-6">
      <CardSection
        title="屏蔽策略"
        description="在统计窗口内达到阈值次回空即触发屏蔽，屏蔽持续设定时长后自动解除"
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
                启用回空屏蔽
              </Label>
              <p class="mt-1 text-xs text-muted-foreground">
                关闭后不记录空响应计数，也不屏蔽任何来源
              </p>
            </div>
            <Switch v-model="shieldEnabled" />
          </div>

          <div class="grid grid-cols-1 gap-4 sm:grid-cols-3">
            <div>
              <Label
                for="shield-threshold"
                class="block text-sm font-medium"
              >
                触发阈值（次）
              </Label>
              <Input
                id="shield-threshold"
                v-model.number="threshold"
                type="number"
                min="1"
                class="mt-1"
              />
              <p class="mt-1 text-xs text-muted-foreground">
                窗口内空响应达到该次数即屏蔽（单个请求重试只计 1 次）
              </p>
            </div>
            <div>
              <Label
                for="shield-window"
                class="block text-sm font-medium"
              >
                统计窗口（秒）
              </Label>
              <Input
                id="shield-window"
                v-model.number="windowSecs"
                type="number"
                min="1"
                class="mt-1"
              />
              <p class="mt-1 text-xs text-muted-foreground">
                计数仅在窗口内有效
              </p>
            </div>
            <div>
              <Label
                for="shield-block"
                class="block text-sm font-medium"
              >
                屏蔽时长（秒）
              </Label>
              <Input
                id="shield-block"
                v-model.number="blockSecs"
                type="number"
                min="1"
                class="mt-1"
              />
              <p class="mt-1 text-xs text-muted-foreground">
                默认 300 秒（5 分钟）
              </p>
            </div>
          </div>

          <div class="flex items-center justify-between gap-4 rounded-lg border border-border/70 px-4 py-3">
            <div>
              <Label class="text-sm font-medium">
                按客户端密钥隔离
              </Label>
              <p class="mt-1 text-xs text-muted-foreground">
                开启后屏蔽键按客户端 API Key 隔离，避免一个用户的回空误伤其他用户的相同请求
              </p>
            </div>
            <Switch v-model="scopeByClient" />
          </div>
        </div>
      </CardSection>

      <CardSection
        title="手动封禁"
        description="直接封禁指定的会话或请求指纹（用于精准封禁破限来源；封禁时长使用上方“屏蔽时长”）"
      >
        <div class="flex flex-col gap-3 sm:flex-row sm:items-end">
          <div class="flex-1">
            <Label
              for="manual-block-type"
              class="block text-sm font-medium"
            >
              封禁类型
            </Label>
            <Select
              v-model="manualBlockType"
              class="mt-1"
            >
              <SelectTrigger
                id="manual-block-type"
                class="h-10"
              >
                <SelectValue placeholder="选择类型" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="session">
                  会话 ID
                </SelectItem>
                <SelectItem value="fingerprint">
                  请求指纹
                </SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div class="flex-[2]">
            <Label
              for="manual-block-value"
              class="block text-sm font-medium"
            >
              {{ manualBlockType === 'session' ? '会话 ID' : '请求指纹（64 位十六进制）' }}
            </Label>
            <Input
              id="manual-block-value"
              v-model="manualBlockValue"
              :placeholder="manualBlockType === 'session' ? '例如 session-abc123' : '例如 9f86d081...'"
              class="mt-1 font-mono text-xs"
            />
          </div>
          <Button
            size="sm"
            class="h-10"
            :disabled="manualBlocking || manualBlockValue.trim() === ''"
            @click="manualBlock"
          >
            <Ban class="mr-1 h-3.5 w-3.5" />
            {{ manualBlocking ? '封禁中...' : '封禁' }}
          </Button>
        </div>
      </CardSection>

      <CardSection
        title="当前被屏蔽来源"
        description="屏蔽到期后自动解除；也可手动解除"
      >
        <template #actions>
          <Button
            size="sm"
            variant="outline"
            :disabled="loadingBlocked"
            @click="loadBlocked"
          >
            {{ loadingBlocked ? '刷新中...' : '刷新' }}
          </Button>
        </template>

        <div
          v-if="!shieldEnabled"
          class="rounded-md border border-border px-3 py-4 text-center text-sm text-muted-foreground"
        >
          回空屏蔽未启用
        </div>
        <div
          v-else-if="loadingBlocked && blocked.length === 0"
          class="rounded-md border border-border px-3 py-4 text-center text-sm text-muted-foreground"
        >
          加载中...
        </div>
        <div
          v-else-if="blocked.length === 0"
          class="rounded-md border border-border px-3 py-4 text-center text-sm text-muted-foreground"
        >
          当前没有被屏蔽的会话 / 请求
        </div>
        <Table v-else>
          <TableHeader>
            <TableRow>
              <TableHead class="h-10">
                类型
              </TableHead>
              <TableHead class="h-10">
                标识
              </TableHead>
              <TableHead class="h-10">
                回空次数
              </TableHead>
              <TableHead class="h-10">
                剩余时间
              </TableHead>
              <TableHead class="h-10 text-right">
                操作
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow
              v-for="entry in blocked"
              :key="entry.key"
            >
              <TableCell class="py-3">
                <div class="flex items-center gap-1">
                  <Badge :variant="entry.kind === 'session' ? 'default' : 'secondary'">
                    {{ entry.kind === 'session' ? '会话' : '请求指纹' }}
                  </Badge>
                  <Badge
                    v-if="entry.manual"
                    variant="warning"
                  >
                    手动
                  </Badge>
                </div>
              </TableCell>
              <TableCell
                class="max-w-[280px] truncate py-3 font-mono text-xs"
                :title="entry.key"
              >
                {{ entry.key }}
              </TableCell>
              <TableCell class="py-3 text-sm tabular-nums">
                {{ entry.strikes > 0 ? entry.strikes : '-' }}
              </TableCell>
              <TableCell class="py-3 text-sm tabular-nums">
                {{ formatRemaining(entry.remaining_secs) }}
              </TableCell>
              <TableCell class="py-3 text-right">
                <Button
                  size="sm"
                  variant="outline"
                  :disabled="unblocking === entry.key"
                  @click="unblock(entry.key)"
                >
                  {{ unblocking === entry.key ? '解除中...' : '解除' }}
                </Button>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
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
  Switch,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui'
import { PageHeader, PageContainer, CardSection } from '@/components/layout'
import { adminApi } from '@/api/admin'
import { shieldApi, type ShieldBlockedEntry } from '@/api/diagnostics'
import { Ban } from 'lucide-vue-next'
import { useToast } from '@/composables/useToast'
import { parseApiError } from '@/utils/errorParser'
import { log } from '@/utils/logger'

const CONFIG_KEY = 'empty_response_shield'

const { success, error } = useToast()

const saving = ref(false)
const shieldEnabled = ref(false)
const threshold = ref(3)
const windowSecs = ref(600)
const blockSecs = ref(300)
const scopeByClient = ref(true)

const loadingBlocked = ref(false)
const unblocking = ref<string | null>(null)
const blocked = ref<ShieldBlockedEntry[]>([])

// 手动封禁表单
const manualBlockType = ref<'session' | 'fingerprint'>('session')
const manualBlockValue = ref('')
const manualBlocking = ref(false)

onMounted(async () => {
  await loadConfig()
  void loadBlocked()
})

async function loadConfig() {
  try {
    const detail = await adminApi.getSystemConfig(CONFIG_KEY, { cacheTtlMs: 0 })
    const value = (detail.value ?? null) as null | {
      enabled?: boolean
      threshold?: number
      window_secs?: number
      block_secs?: number
      scope_by_client?: boolean
    }
    shieldEnabled.value = value?.enabled === true
    threshold.value = typeof value?.threshold === 'number' && value.threshold > 0 ? value.threshold : 3
    windowSecs.value = typeof value?.window_secs === 'number' && value.window_secs > 0 ? value.window_secs : 600
    blockSecs.value = typeof value?.block_secs === 'number' && value.block_secs > 0 ? value.block_secs : 300
    scopeByClient.value = value?.scope_by_client !== false
  } catch (err) {
    const status = (err as { response?: { status?: number } })?.response?.status
    if (status !== 404) {
      error(parseApiError(err, '加载回空屏蔽配置失败'))
      log.error('加载回空屏蔽配置失败:', err)
    }
  }
}

async function loadBlocked() {
  loadingBlocked.value = true
  try {
    const status = await shieldApi.getStatus()
    blocked.value = status.blocked || []
  } catch (err) {
    error(parseApiError(err, '加载屏蔽列表失败'))
    log.error('加载屏蔽列表失败:', err)
  } finally {
    loadingBlocked.value = false
  }
}

async function saveConfig() {
  saving.value = true
  try {
    const payload = {
      enabled: shieldEnabled.value,
      threshold: positiveInt(threshold.value, 3),
      window_secs: positiveInt(windowSecs.value, 600),
      block_secs: positiveInt(blockSecs.value, 300),
      scope_by_client: scopeByClient.value,
    }
    await adminApi.updateSystemConfig(CONFIG_KEY, payload, '回空屏蔽配置')
    success('回空屏蔽配置已保存')
    await loadBlocked()
  } catch (err) {
    error(parseApiError(err, '保存回空屏蔽配置失败'))
    log.error('保存回空屏蔽配置失败:', err)
  } finally {
    saving.value = false
  }
}

async function unblock(key: string) {
  unblocking.value = key
  try {
    await shieldApi.unblock(key)
    success('已解除屏蔽')
    await loadBlocked()
  } catch (err) {
    error(parseApiError(err, '解除屏蔽失败'))
    log.error('解除屏蔽失败:', err)
  } finally {
    unblocking.value = null
  }
}

// 手动封禁指定会话/指纹（服务端按“按客户端密钥隔离”设置推导完整屏蔽键）
async function manualBlock() {
  const value = manualBlockValue.value.trim()
  if (!value || manualBlocking.value) return
  manualBlocking.value = true
  try {
    const params = manualBlockType.value === 'session'
      ? { session: value }
      : { fingerprint: value }
    const result = await shieldApi.block(params)
    success(`已封禁 ${result.key}（${result.block_secs} 秒）`)
    manualBlockValue.value = ''
    await loadBlocked()
  } catch (err) {
    error(parseApiError(err, '手动封禁失败'))
    log.error('手动封禁失败:', err)
  } finally {
    manualBlocking.value = false
  }
}

function positiveInt(value: unknown, fallback: number): number {
  const parsed = Math.floor(Number(value))
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback
}

function formatRemaining(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '即将解除'
  const minutes = Math.floor(seconds / 60)
  const rest = seconds % 60
  if (minutes <= 0) return `${rest} 秒`
  return `${minutes} 分 ${rest} 秒`
}
</script>
