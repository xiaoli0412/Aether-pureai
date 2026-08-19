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
              追踪上游失败请求的取证记录，定位滥用来源
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
              <Badge
                v-if="statusCodeFor(record) !== null"
                :variant="getStatusCodeVariant(statusCodeFor(record))"
                class="shrink-0"
              >
                {{ statusCodeFor(record) }}
              </Badge>
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

    <!-- 详情加载指示器 -->
    <div
      v-if="detailLoading && !detail"
      class="fixed inset-0 bg-black/30 flex items-center justify-center z-50"
    >
      <div class="animate-spin rounded-full h-8 w-8 border-b-2 border-primary" />
    </div>

    <!-- 取证详情对话框 -->
    <div
      v-if="detail"
      class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
      @click="closeDetail"
    >
      <Card
        class="max-w-4xl w-full mx-4 max-h-[85vh] overflow-y-auto"
        @click.stop
      >
        <div class="p-6 space-y-4">
          <div class="flex justify-between items-center">
            <div class="min-w-0">
              <h3 class="text-lg font-medium">
                诊断详情
              </h3>
              <p
                class="text-xs text-muted-foreground truncate"
                :title="detail.request_id"
              >
                请求 ID：{{ detail.request_id }}
              </p>
            </div>
            <Button
              variant="ghost"
              size="sm"
              @click="closeDetail"
            >
              <X class="h-4 w-4" />
            </Button>
          </div>

          <!-- 概览 -->
          <div class="grid grid-cols-2 sm:grid-cols-3 gap-3 text-sm">
            <div>
              <Label>类型</Label>
              <div class="mt-1">
                <Badge :variant="kindBadgeVariant(diagnosticKind)">
                  {{ kindLabel(diagnosticKind) }}
                </Badge>
              </div>
            </div>
            <div>
              <Label>上游状态</Label>
              <p class="mt-1">
                <Badge
                  v-if="detailUpstreamStatus !== null"
                  :variant="getStatusCodeVariant(detailUpstreamStatus)"
                >
                  {{ detailUpstreamStatus }}
                </Badge>
                <span v-else>-</span>
              </p>
            </div>
            <div>
              <Label>最终状态</Label>
              <p class="mt-1">
                {{ detail.status || '-' }}
                <span v-if="detail.status_code !== null">（{{ detail.status_code }}）</span>
              </p>
            </div>
            <div>
              <Label>模型</Label>
              <p class="mt-1">
                {{ detail.model || '-' }}
                <span
                  v-if="detail.target_model"
                  class="text-xs text-muted-foreground"
                > → {{ detail.target_model }}</span>
              </p>
            </div>
            <div>
              <Label>提供商</Label>
              <p class="mt-1">
                {{ detail.provider_name || '-' }}
              </p>
            </div>
            <div>
              <Label>格式 / 流式</Label>
              <p class="mt-1">
                {{ detail.api_format || '-' }} / {{ detail.is_stream ? '流式' : '同步' }}
              </p>
            </div>
            <div>
              <Label>用户</Label>
              <p class="mt-1">
                {{ detail.user_id || '-' }}
              </p>
            </div>
            <div>
              <Label>密钥</Label>
              <p
                class="mt-1 truncate"
                :title="detail.api_key_id || ''"
              >
                {{ detail.api_key_id || '-' }}
              </p>
            </div>
            <div>
              <Label>会话ID</Label>
              <p
                class="mt-1 truncate font-mono text-xs"
                :title="detail.session_id || detail.request_fingerprint || ''"
              >
                {{ detail.session_id || detail.request_fingerprint || '-' }}
              </p>
            </div>
            <div>
              <Label>时间</Label>
              <p class="mt-1">
                {{ formatDateTime(detail.created_at) }}
              </p>
            </div>
          </div>

          <div v-if="diagnosticMessage">
            <Label>诊断信息</Label>
            <p class="mt-1 text-sm whitespace-pre-wrap break-words">
              {{ diagnosticMessage }}
            </p>
          </div>

          <div v-if="detail.error_message">
            <Label>错误消息</Label>
            <p class="mt-1 text-sm text-destructive whitespace-pre-wrap break-words">
              {{ detail.error_message }}
            </p>
          </div>

          <Separator />

          <!-- AI 摘要 -->
          <div class="space-y-2">
            <div class="flex items-center justify-between gap-2">
              <div class="flex items-center gap-2">
                <Sparkles class="h-4 w-4 text-primary" />
                <Label>AI 摘要</Label>
                <span
                  v-if="summaryMeta"
                  class="text-xs text-muted-foreground"
                >
                  {{ summaryMeta }}
                </span>
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
                  {{ summaryText ? '查看摘要' : '生成摘要' }}
                </Button>
                <Button
                  v-if="summaryText"
                  size="sm"
                  variant="ghost"
                  :disabled="summarizing"
                  title="绕过缓存重新生成"
                  @click="generateSummary(true)"
                >
                  <RefreshCw class="h-3.5 w-3.5" />
                </Button>
              </div>
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
              尚未生成摘要。点击"生成摘要"调用已配置的 AI 摘要器（未配置时返回 503）。
            </p>
          </div>

          <Separator />

          <!-- 诊断标记原始 JSON -->
          <div v-if="detail.error_diagnostic">
            <Label>诊断标记（error_diagnostic）</Label>
            <pre class="mt-1 text-xs bg-muted p-3 rounded-md overflow-x-auto max-h-64 overflow-y-auto">{{ prettyJson(detail.error_diagnostic) }}</pre>
          </div>

          <!-- 四段取证捕获 -->
          <div
            v-for="capture in captureSections"
            :key="capture.key"
            class="border border-border/60 rounded-md"
          >
            <button
              class="w-full flex items-center justify-between px-3 py-2 text-sm font-medium hover:bg-muted/30 transition-colors"
              @click="toggleCapture(capture.key)"
            >
              <span class="flex items-center gap-2">
                <component
                  :is="captureOpen[capture.key] ? ChevronDown : ChevronRight"
                  class="h-4 w-4"
                />
                {{ capture.label }}
              </span>
              <span class="text-xs text-muted-foreground">
                {{ capturePresent(capture.value) ? '有数据' : '无数据' }}
              </span>
            </button>
            <div
              v-if="captureOpen[capture.key]"
              class="px-3 pb-3 space-y-2"
            >
              <div v-if="capture.value?.headers">
                <Label class="text-xs">请求/响应头</Label>
                <pre class="mt-1 text-xs bg-muted p-2 rounded-md overflow-x-auto max-h-40 overflow-y-auto">{{ prettyJson(capture.value.headers) }}</pre>
              </div>
              <div v-if="capture.value?.body !== null && capture.value?.body !== undefined">
                <Label class="text-xs">正文</Label>
                <pre class="mt-1 text-xs bg-muted p-2 rounded-md overflow-x-auto max-h-72 overflow-y-auto">{{ prettyJson(capture.value.body) }}</pre>
              </div>
              <p
                v-if="!capturePresent(capture.value)"
                class="text-xs text-muted-foreground pb-1"
              >
                该段未捕获任何数据。
              </p>
            </div>
          </div>
        </div>
      </Card>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, reactive, onMounted, onBeforeUnmount, computed } from 'vue'
import {
  Card,
  Button,
  Badge,
  Separator,
  Label,
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
import {
  diagnosticsApi,
  diagnosticErrorMessage,
  type DiagnosticRecord,
  type DiagnosticDetail,
  type DiagnosticCapture
} from '@/api/diagnostics'
import {
  Search,
  Key,
  X,
  FilterX,
  Sparkles,
  RefreshCw,
  Loader2,
  ChevronDown,
  ChevronRight
} from 'lucide-vue-next'
import { useRowClick } from '@/composables/useRowClick'
import { useToast } from '@/composables/useToast'
import { log } from '@/utils/logger'

const { toast } = useToast()
const { handleMouseDown, shouldTriggerRowClick } = useRowClick()

const loading = ref(false)
const records = ref<DiagnosticRecord[]>([])
const detail = ref<DiagnosticDetail | null>(null)
const detailLoading = ref(false)
const summarizing = ref(false)
const summaryText = ref('')
const summaryError = ref<string | null>(null)
const summaryMeta = ref('')
let recordsRequestId = 0

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

const diagnosticKind = computed(() => {
  const kind = detail.value?.error_diagnostic?.kind
  return typeof kind === 'string' ? kind : null
})

const diagnosticMessage = computed(() => {
  const message = detail.value?.error_diagnostic?.message
  if (typeof message === 'string' && message.length > 0) return message
  return ''
})

const detailUpstreamStatus = computed(() => {
  const status = detail.value?.error_diagnostic?.upstream_status
  return typeof status === 'number' ? status : null
})

const captureSections = computed(() => {
  const value = detail.value
  if (!value) return []
  return [
    { key: 'request', label: '客户端请求', value: value.request },
    { key: 'provider_request', label: '上游请求', value: value.provider_request },
    { key: 'response', label: '上游响应', value: value.response },
    { key: 'client_response', label: '客户端响应', value: value.client_response },
  ] as Array<{ key: string; label: string; value: DiagnosticCapture | undefined }>
})

const captureOpen = reactive<Record<string, boolean>>({
  request: false,
  provider_request: false,
  response: true,
  client_response: false,
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

async function openDetail(record: DiagnosticRecord) {
  const requestId = ++recordsRequestId
  detail.value = null
  summaryText.value = ''
  summaryError.value = null
  summaryMeta.value = ''
  detailLoading.value = true
  try {
    const payload = await diagnosticsApi.getDiagnosticDetail(record.request_id, true)
    if (requestId !== recordsRequestId) return
    detail.value = payload
    hydrateSummaryFromDetail()
  } catch (error) {
    if (requestId !== recordsRequestId) return
    log.error('获取诊断详情失败:', error)
    toast({
      title: '获取诊断详情失败',
      description: diagnosticErrorMessage(error),
      variant: 'destructive'
    })
  } finally {
    if (requestId === recordsRequestId) {
      detailLoading.value = false
    }
  }
}

function closeDetail() {
  detail.value = null
  summaryText.value = ''
  summaryError.value = null
  summaryMeta.value = ''
}

function hydrateSummaryFromDetail() {
  const diagnostic = detail.value?.error_diagnostic
  const summary = diagnostic?.summary
  if (typeof summary === 'string' && summary.length > 0) {
    summaryText.value = summary
    const summarizedAt = diagnostic?.summarized_at_unix_secs
    summaryMeta.value = typeof summarizedAt === 'number'
      ? `（已持久化 · ${formatDateTime(new Date(summarizedAt * 1000).toISOString())}）`
      : '（已持久化）'
  }
}

async function generateSummary(refresh: boolean) {
  const target = detail.value
  if (!target || summarizing.value) return
  summarizing.value = true
  summaryError.value = null
  try {
    const result = await diagnosticsApi.summarizeDiagnostic(target.request_id, refresh)
    summaryText.value = result.summary
    summaryMeta.value = result.cached
      ? `（缓存 · ${formatDateTime(new Date(result.summarized_at_unix_secs * 1000).toISOString())}）`
      : `（${result.persisted ? '已持久化' : '未持久化'} · ${formatDateTime(new Date(result.summarized_at_unix_secs * 1000).toISOString())}）`
    if (detail.value) {
      const diagnostic = detail.value.error_diagnostic ?? {}
      diagnostic.summary = result.summary
      diagnostic.summarized_at_unix_secs = result.summarized_at_unix_secs
      detail.value.error_diagnostic = diagnostic
    }
  } catch (error) {
    log.error('生成 AI 摘要失败:', error)
    summaryError.value = diagnosticErrorMessage(error)
  } finally {
    summarizing.value = false
  }
}

function toggleCapture(key: string) {
  captureOpen[key] = !captureOpen[key]
}

function capturePresent(capture: DiagnosticCapture | undefined): boolean {
  if (!capture) return false
  const hasHeaders = capture.headers !== null && capture.headers !== undefined
  const hasBody = capture.body !== null && capture.body !== undefined
  return hasHeaders || hasBody
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

function prettyJson(value: unknown): string {
  try {
    if (typeof value === 'string') return value
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
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
