import apiClient from './client'

export type DiagnosticKind = 'empty_response' | 'upstream_4xx' | 'upstream_5xx'

export interface DiagnosticRecord {
  usage_id: string
  request_id: string
  kind: string | null
  upstream_status: number | null
  classification: string | null
  decision: string | null
  message: string | null
  session_id: string | null
  request_fingerprint: string | null
  user_id: string | null
  api_key_id: string | null
  model: string | null
  provider_name: string | null
  status: string | null
  status_code: number | null
  created_at: string
}

export interface DiagnosticsListResponse {
  records: DiagnosticRecord[]
  total: number
  limit: number
  offset: number
}

export interface DiagnosticsListFilters {
  kind?: string
  user_id?: string
  api_key_id?: string
  from?: number
  to?: number
  limit?: number
  offset?: number
  newest_first?: boolean
}

export interface DiagnosticCapture {
  headers: Record<string, unknown> | null
  body: unknown
}

export interface DiagnosticDetail {
  usage_id: string
  request_id: string
  error_diagnostic: Record<string, unknown> | null
  status: string | null
  status_code: number | null
  error_message: string | null
  error_category: string | null
  session_id: string | null
  request_fingerprint: string | null
  user_id: string | null
  api_key_id: string | null
  model: string | null
  target_model: string | null
  provider_name: string | null
  provider_id: string | null
  provider_endpoint_id: string | null
  provider_api_key_id: string | null
  api_format: string | null
  is_stream: boolean | null
  created_at: string
  request: DiagnosticCapture
  provider_request: DiagnosticCapture
  response: DiagnosticCapture
  client_response: DiagnosticCapture
}

export interface DiagnosticsSummary {
  request_id: string
  summary: string
  cached: boolean
  persisted: boolean
  summarized_at_unix_secs: number
}

export const diagnosticsApi = {
  // 错误诊断事件列表 (管理员)
  async listDiagnostics(filters?: DiagnosticsListFilters): Promise<DiagnosticsListResponse> {
    const response = await apiClient.get('/api/admin/diagnostics', { params: filters })
    const data = response.data ?? {}
    return {
      records: (data.records ?? []) as DiagnosticRecord[],
      total: (data.total as number) ?? 0,
      limit: (data.limit as number) ?? 0,
      offset: (data.offset as number) ?? 0
    }
  },

  // 单条诊断事件的完整取证详情 (管理员)
  async getDiagnosticDetail(requestId: string, includeBodies: boolean = true): Promise<DiagnosticDetail> {
    const response = await apiClient.get(`/api/admin/diagnostics/${encodeURIComponent(requestId)}`, {
      params: { include_bodies: includeBodies }
    })
    return response.data
  },

  // 触发/复用 AI 摘要 (管理员)；refresh=true 时绕过 24 小时缓存
  async summarizeDiagnostic(requestId: string, refresh: boolean = false): Promise<DiagnosticsSummary> {
    const response = await apiClient.post(
      `/api/admin/diagnostics/${encodeURIComponent(requestId)}/summarize`,
      null,
      { params: { refresh } }
    )
    return response.data
  }
}

// 从后端错误载荷中提取可读的错误描述（503/404/502 均返回 detail 字段）
export function diagnosticErrorMessage(error: unknown): string {
  if (error && typeof error === 'object' && 'response' in error) {
    const data = (error as { response?: { data?: { detail?: unknown } } }).response?.data
    if (data && typeof data.detail === 'string' && data.detail.length > 0) {
      return data.detail
    }
  }
  return error instanceof Error ? error.message : String(error)
}

// ---- 回空屏蔽（empty-response shield）管理端 ----

export interface ShieldBlockedEntry {
  key: string
  kind: 'session' | 'fingerprint' | string
  remaining_secs: number
}

export interface ShieldStatus {
  installed: boolean
  config: { threshold: number; window_secs: number; block_secs: number } | null
  blocked: ShieldBlockedEntry[]
  total: number
}

export const shieldApi = {
  // 查看屏蔽状态与被屏蔽键列表
  async getStatus(): Promise<ShieldStatus> {
    const response = await apiClient.get('/api/admin/empty-response-shield')
    return response.data
  },

  // 手动解除某个会话/指纹的屏蔽
  async unblock(key: string): Promise<{ key: string; unblocked: boolean }> {
    const response = await apiClient.delete(
      `/api/admin/empty-response-shield/${encodeURIComponent(key)}`
    )
    return response.data
  }
}
