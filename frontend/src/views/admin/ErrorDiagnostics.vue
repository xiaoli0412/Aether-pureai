<template>
  <div class="space-y-6 pb-8">
    <!-- 错误诊断列表 -->
    <Card
      variant="default"
      class="overflow-hidden"
    >
      <!-- 标题和操作栏 -->
      <div class="px-4 sm:px-6 py-3 sm:py-3.5 border-b border-border/60">
        <div class="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3 sm:gap-4">
          <div class="shrink-0">
            <h3 class="text-sm sm:text-base font-semibold">
              错误诊断
            </h3>
            <p class="text-xs text-muted-foreground mt-0.5">
              追踪上游失败请求的取证记录，定位滥用来源；点击行查看完整请求详情
            </p>
          </div>
          <div class="flex flex-wrap items-center gap-2">
            <!-- 用户 ID 搜索框 -->
            <div class="relative">
              <Search class="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground z-10 pointer-events-none" />
              <Input
                id="diagnostics-user-search"
                v-model="searchQuery"
                placeholder="搜索用户 ID..."
                class="w-32 sm:w-52 h-8 text-sm pl-8"
                @input="handleSearchChange"
              />
            </div>
            <!-- 密钥 ID 搜索框 -->
            <div class="relative">
              <Key class="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground z-10 pointer-events-none" />
              <Input
                id="diagnostics-key-search"
                v-model="apiKeyQuery"
                placeholder="搜索密钥 ID..."
                class="w-32 sm:w-52 h-8 text-sm pl-8"
                @input="handleSearchChange"
              />
            </div>
            <!-- 分隔线 -->
            <div class="hidden sm:block h-4 w-px bg-border" />
            <!-- 诊断类型筛选 -->
            <div class="xl:hidden">
              <Select
                v-model="filters.kind"
                @update:model-value="handleKindChange"
              >
                <SelectTrigger class="w-24 sm:w-40 h-8 border-border/60">
                  <SelectValue placeholder="全部类型" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem
                    v-for="option in kindFilterOptions"
                    :key="option.value"
                    :value="option.value"
                  >
                    {{ option.label }}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <!-- 时间范围筛选 -->
            <div class="xl:hidden">
              <Select
                v-model="filtersDaysString"
                @update:model-value="handleDaysChange"
              >
                <SelectTrigger class="w-20 sm:w-28 h-8 border-border/60">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem
                    v-for="option in daysFilterOptions"
                    :key="option.value"
                    :value="option.value"
                  >
                    {{ option.label }}
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
            <!-- 重置筛选 -->
            <Button
              v-if="hasActiveFilters"
              variant="ghost"
              size="icon"
              class="h-8 w-8"
              title="重置筛选"
              @click="handleResetFilters"
            >
              <FilterX class="w-3.5 h-3.5" />
            </Button>
            <!-- 刷新按钮 -->
            <RefreshButton
              :loading="loading"
              @click="loadRecords"
            />
          </div>
        </div>
      </div>

      <div
        v-if="loading"
        class="flex items-center justify-center py-12"
      >
        <div class="animate-spin rounded-full h-8 w-8 border-b-2 border-primary" />
      </div>

      <div
        v-else-if="records.length === 0"
        class="text-center py-12 text-muted-foreground"
      >
        暂无诊断记录
      </div>

      <div v-else>
        <Table class="hidden xl:table">
          <TableHeader>
            <TableRow class="border-b border-border/60 hover:bg-transparent">
              <SortableTableHead
                class="h-12 font-semibold"
                column-key="created_at"
                :sortable="false"
                :filter-active="filters.days !== 7"
                filter-title="筛选时间范围"
                filter-content-class="w-32 p-1 rounded-2xl border-border bg-card text-foreground shadow-2xl backdrop-blur-xl"
              >
                时间
                <template #filter="{ close }">
                  <TableFilterMenu
                    :model-value="filtersDaysString"
                    :options="daysFilterOptions"
                    @update:model-value="handleDaysChange"
                    @select="close"
                  />
                </template>
              </SortableTableHead>
              <SortableTableHead
                class="h-12 font-semibold"
                column-key="kind"
                :sortable="false"
                :filter-active="filters.kind !== '__all__'"
                filter-title="筛选诊断类型"
                filter-content-class="w-48 p-1 rounded-2xl border-border bg-card text-foreground shadow-2xl backdrop-blur-xl"
              >
                类型
                <template #filter="{ close }">
                  <TableFilterMenu
                    :model-value="filters.kind"
                    :options="kindFilterOptions"
                    @update:model-value="handleKindChange"
                    @select="close"
                  />
                </template>
              </SortableTableHead>
              <TableHead class="h-12 font-semibold">
                模型
              </TableHead>
              <TableHead class="h-12 font-semibold">
                用户 / 密钥
              </TableHead>
              <TableHead class="h-12 font-semibold">
                提供商
              </TableHead>
              <TableHead class="h-12 font-semibold">
                上游状态
              </TableHead>
              <TableHead class="h-12 font-semibold">
                信息
              </TableHead>
              <TableHead class="h-12 font-semibold text-right">
                操作
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow
              v-for="record in records"
              :key="record.usage_id"
              class="cursor-pointer border-b border-border/40 hover:bg-muted/30 transition-colors"
              @mousedown="handleMouseDown"
              @click="handleRowClick($event, record)"
            >
              <TableCell class="text-xs py-4">
                {{ formatDateTime(record.created_at) }}
              </TableCell>

              <TableCell class="py-4">
                <Badge :variant="kindBadgeVariant(record.kind)">
                  {{ kindLabel(record.kind) }}
                </Badge>
              </TableCell>

              <TableCell class="py-4 text-sm">
                {{ record.model || '-' }}
              </TableCell>

              <TableCell class="py-4">
                <div class="flex flex-col">
                  <span class="text-sm font-medium">
                    {{ record.user_id || '-' }}
                  </span>
                  <span
                    v-if="record.api_key_id"
                    class="text-xs text-muted-foreground truncate max-w-[8rem]"
                    :title="record.api_key_id"
                  >
                    {{ record.api_key_id }}
                  </span>
                </div>
              </TableCell>

              <TableCell class="py-4 text-sm">
                {{ record.provider_name || '-' }}
              </TableCell>

              <TableCell class="py-4">
                <Badge
                  v-if="statusCodeFor(record) !== null"
                  :variant="getStatusCodeVariant(statusCodeFor(record))"
                >
                  {{ statusCodeFor(record) }}
                </Badge>
                <span v-else>-</span>
              </TableCell>

              <TableCell
                class="max-w-xs truncate py-4 text-xs text-muted-foreground"
                :title="record.message || ''"
              >
                {{ record.message || '无信息' }}
              </TableCell>

              <TableCell class="py-4 text-right">
                <Button
                  variant="ghost"
                  size="sm"
                  class="h-8 px-2 text-xs"
                  title="生成/查看 AI 摘要"
                  @click.stop="openSummary(record)"
                >
                  <Sparkles class="h-3.5 w-3.5 mr-1" />
                  AI 摘要
                </Button>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>

        <!-- 移动端卡片列表 -->
        <div
          v-if="records.length > 0"
          class="xl:hidden divide-y divide-border/40"
        >
          <div
            v-for="record in records"
            :key="record.usage_id"
            class="p-4 space-y-2 hover:bg-muted/30 cursor-pointer transition-colors"
            @click="openDetail(record)"
          >
            <div class="flex items-start justify-between gap-3">
              <div class="flex-1 min-w-0">
                <Badge :variant="kindBadgeVariant(record.kind)">
                  {{ kindLabel(record.kind) }}
                </Badge>
                <div class="text-xs text-muted-foreground mt-1.5">
                  {{ formatDateTime(record.created_at) }}
                </div>
              </div>
              <Button
                variant="ghost"
                size="sm"
                class="h-8 px-2 text-xs shrink-0"
                @click.stop="openSummary(record)"
              >
                <Sparkles class="h-3.5 w-3.5 mr-1" />
                摘要
              </Button>
            </div>
            <div class="text-sm">
              {{ record.model || '-' }} · {{ record.provider_name || '未知提供商' }}
            </div>
            <div class="text-xs text-muted-foreground">
              用户 {{ record.user_id || '-' }}
            </div>
            <div
              class="text-xs text-muted-foreground truncate"
              :title="record.message || ''"
            >
              {{ record.message || '无信息' }}
            </div>
          </div>
        </div>

        <!-- 分页控件 -->
        <Pagination
          :current="currentPage"
          :total="totalRecords"
          :page-size="pageSize"
          :page-size-options="[10, 20, 50, 100]"
          cache-key="diagnostics-page-size"
          @update:current="handlePageChange"
          @update:page-size="pageSize = $event; currentPage = 1; loadRecords()"
        />
      </div>
    </Card>

    <!-- 完整请求详情抽屉（与使用记录同款多视图呈现） -->
    <RequestDetailDrawer
      :is-open="detailOpen"
      :request-id="selectedUsageId"
      @close="detailOpen = false"
    />

    <!-- AI 摘要弹窗 -->
    <div
      v-if="summaryRecord"
      class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
      @click="closeSummary"
    >
      <Card
        class="max-w-2xl w-full mx-4 max-h-[80vh] overflow-y-auto"
        @click.stop
      >
        <div class="p-6 space-y-4">
          <div class="flex justify-between items-center">
            <div class="min-w-0">
              <h3 class="text-lg font-medium flex items-center gap-2">
                <Sparkles class="h-4 w-4 text-primary" />
                AI 摘要
              </h3>
              <p
                class="text-xs text-muted-foreground truncate"
                :title="summaryRecord.request_id"
              >
                {{ kindLabel(summaryRecord.kind) }} · {{ summaryRecord.request_id }}
              </p>
            </div>
            <Button
              variant="ghost"
              size="sm"
              @click="closeSummary"
            >
              <X class="h-4 w-4" />
            </Button>
          </div>

          <div
            v-if="summaryRecord.message"
            class="text-xs text-muted-foreground bg-muted/50 rounded-md px-3 py-2 whitespace-pre-wrap break-words"
          >
            {{ summaryRecord.message }}
          </div>

          <div class="flex items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              :disabled="summarizing"
              @click="generateSummary(false)"
            >
              <Loader2
                v-if="summarizing"
                class="h-3.5 w-3.5 mr-1 animate-spin"
              />
              <Sparkles
                v-else
                class="h-3.5 w-3.5 mr-1"
              />
              {{ summaryText ? '重新获取' : '生成摘要' }}
            </Button>
            <Button
              v-if="summaryText"
              size="sm"
              variant="ghost"
              :disabled="summarizing"
              title="绕过缓存强制重新生成"
              @click="generateSummary(true)"
            >
              <RefreshCw class="h-3.5 w-3.5 mr-1" />
              强制重新生成
            </Button>
            <span
              v-if="summaryMeta"
              class="text-xs text-muted-foreground"
            >
              {{ summaryMeta }}
            </span>
          </div>

          <p
            v-if="summaryError"
            class="text-sm text-destructive whitespace-pre-wrap break-words"
          >
            {{ summaryError }}
          </p>
          <p
            v-else-if="summaryText"
            class="text-sm bg-muted p-3 rounded-md whitespace-pre-wrap break-words"
          >
            {{ summaryText }}
          </p>
          <p
            v-else-if="!summarizing"
            class="text-xs text-muted-foreground"
          >
            尚未生成摘要。点击"生成摘要"调用已配置的 AI 摘要器；未配置时请先在 模块管理 → 错误诊断 中完成配置。
          </p>
        </div>
      </Card>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, computed } from 'vue'
import {
  Card,
  Button,
  Badge,
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHead,
  SortableTableHead,
  TableFilterMenu,
  TableCell,
  Input,
  Pagination,
  RefreshButton
} from '@/components/ui'
import { RequestDetailDrawer } from '@/features/usage/components'
import {
  diagnosticsApi,
  diagnosticErrorMessage,
  type DiagnosticRecord
} from '@/api/diagnostics'
import {
  Search,
  Key,
  X,
  FilterX,
  Sparkles,
  RefreshCw,
  Loader2
} from 'lucide-vue-next'
import { useRowClick } from '@/composables/useRowClick'
import { useToast } from '@/composables/useToast'
import { log } from '@/utils/logger'

const { toast } = useToast()
const { handleMouseDown, shouldTriggerRowClick } = useRowClick()

const loading = ref(false)
const records = ref<DiagnosticRecord[]>([])
let recordsRequestId = 0

// 详情抽屉（复用使用记录的请求详情组件，多视图呈现请求/响应/头部）
const detailOpen = ref(false)
const selectedUsageId = ref<string | null>(null)

// AI 摘要弹窗
const summaryRecord = ref<DiagnosticRecord | null>(null)
const summarizing = ref(false)
const summaryText = ref('')
const summaryError = ref<string | null>(null)
const summaryMeta = ref('')

const searchQuery = ref('')
const apiKeyQuery = ref('')

const filters = ref({
  userId: '',
  apiKeyId: '',
  kind: '__all__',
  days: 7
})

const filtersDaysString = ref('7')
const kindFilterOptions = [
  { value: '__all__', label: '全部类型' },
  { value: 'empty_response', label: '空响应（200）' },
  { value: 'upstream_4xx', label: '上游 4xx' },
  { value: 'upstream_5xx', label: '上游 5xx' },
  { value: 'empty_response_shield', label: '回空屏蔽' },
]
const daysFilterOptions = [
  { value: '1', label: '1天' },
  { value: '7', label: '7天' },
  { value: '30', label: '30天' },
  { value: '90', label: '90天' },
]

const currentPage = ref(1)
const pageSize = ref(20)
const totalRecords = ref(0)

let loadTimeout: number | null = null
const debouncedLoad = () => {
  if (loadTimeout !== null) clearTimeout(loadTimeout)
  loadTimeout = window.setTimeout(resetAndLoad, 500)
}

const hasActiveFilters = computed(() => {
  return searchQuery.value !== '' ||
    apiKeyQuery.value !== '' ||
    filters.value.kind !== '__all__' ||
    filters.value.days !== 7
})

async function loadRecords() {
  const requestId = ++recordsRequestId
  loading.value = true
  try {
    const offset = (currentPage.value - 1) * pageSize.value
    const now = Math.floor(Date.now() / 1000)

    const data = await diagnosticsApi.listDiagnostics({
      user_id: filters.value.userId || undefined,
      api_key_id: filters.value.apiKeyId || undefined,
      kind: filters.value.kind !== '__all__' ? filters.value.kind : undefined,
      from: now - filters.value.days * 86400,
      to: now,
      newest_first: true,
      limit: pageSize.value,
      offset
    })
    if (requestId !== recordsRequestId) return
    records.value = data.records
    totalRecords.value = data.total
  } catch (error) {
    if (requestId !== recordsRequestId) return
    log.error('获取诊断记录失败:', error)
    toast({
      title: '获取诊断记录失败',
      description: diagnosticErrorMessage(error),
      variant: 'destructive'
    })
    records.value = []
    totalRecords.value = 0
  } finally {
    if (requestId === recordsRequestId) {
      loading.value = false
    }
  }
}

function handleSearchChange() {
  filters.value.userId = searchQuery.value.trim()
  filters.value.apiKeyId = apiKeyQuery.value.trim()
  debouncedLoad()
}

function handleResetFilters() {
  searchQuery.value = ''
  apiKeyQuery.value = ''
  filters.value.userId = ''
  filters.value.apiKeyId = ''
  filters.value.kind = '__all__'
  filters.value.days = 7
  filtersDaysString.value = '7'
  currentPage.value = 1
  loadRecords()
}

function handlePageChange(page: number) {
  currentPage.value = page
  loadRecords()
}

function handleKindChange(value: string) {
  filters.value.kind = value
  resetAndLoad()
}

function handleDaysChange(value: string) {
  filtersDaysString.value = value
  filters.value.days = parseInt(value)
  resetAndLoad()
}

function resetAndLoad() {
  currentPage.value = 1
  loadRecords()
}

function handleRowClick(event: MouseEvent, record: DiagnosticRecord) {
  if (!shouldTriggerRowClick(event)) return
  openDetail(record)
}

// 行点击打开与使用记录一致的请求详情抽屉（按用量记录 ID 拉取完整取证数据）
function openDetail(record: DiagnosticRecord) {
  selectedUsageId.value = record.usage_id
  detailOpen.value = true
}

function openSummary(record: DiagnosticRecord) {
  summaryRecord.value = record
  summaryText.value = ''
  summaryError.value = null
  summaryMeta.value = ''
}

function closeSummary() {
  summaryRecord.value = null
  summaryText.value = ''
  summaryError.value = null
  summaryMeta.value = ''
}

async function generateSummary(refresh: boolean) {
  const target = summaryRecord.value
  if (!target || summarizing.value) return
  summarizing.value = true
  summaryError.value = null
  try {
    const result = await diagnosticsApi.summarizeDiagnostic(target.request_id, refresh)
    summaryText.value = result.summary
    summaryMeta.value = result.cached
      ? `（缓存 · ${formatDateTime(new Date(result.summarized_at_unix_secs * 1000).toISOString())}）`
      : `（${result.persisted ? '已持久化' : '未持久化'} · ${formatDateTime(new Date(result.summarized_at_unix_secs * 1000).toISOString())}）`
  } catch (error) {
    log.error('生成 AI 摘要失败:', error)
    summaryError.value = diagnosticErrorMessage(error)
  } finally {
    summarizing.value = false
  }
}

function statusCodeFor(record: DiagnosticRecord): number | null {
  if (typeof record.upstream_status === 'number') return record.upstream_status
  if (typeof record.status_code === 'number') return record.status_code
  return null
}

function kindLabel(kind: string | null): string {
  const labels: Record<string, string> = {
    'empty_response': '空响应',
    'upstream_4xx': '上游 4xx',
    'upstream_5xx': '上游 5xx',
    'empty_response_shield': '回空屏蔽',
  }
  return (kind && labels[kind]) || kind || '未知'
}

function kindBadgeVariant(kind: string | null): 'default' | 'success' | 'destructive' | 'warning' | 'secondary' {
  if (kind === 'empty_response') return 'warning'
  if (kind === 'upstream_4xx' || kind === 'upstream_5xx') return 'destructive'
  if (kind === 'empty_response_shield') return 'default'
  return 'secondary'
}

function getStatusCodeVariant(statusCode: number | null): 'default' | 'success' | 'destructive' | 'warning' {
  if (statusCode === null) return 'default'
  if (statusCode < 300) return 'success'
  if (statusCode < 400) return 'default'
  if (statusCode < 500) return 'warning'
  return 'destructive'
}

function formatDateTime(dateStr: string): string {
  const date = new Date(dateStr)
  return date.toLocaleString('zh-CN', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit'
  })
}

onMounted(() => {
  loadRecords()
})

onBeforeUnmount(() => {
  if (loadTimeout !== null) {
    clearTimeout(loadTimeout)
    loadTimeout = null
  }
  recordsRequestId += 1
})
</script>
