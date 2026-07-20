import apiClient from './client'

export interface RelayIntegrationConfigStatus {
  instance_id: string
  route_profile: string
  execution_mode: string
  enabled: boolean
  capability_version: string
  revision: number
  updated_at_unix_ms: number
}

export interface RelayIntegrationCredentialStatus {
  credential_revision: number
  transition_expires_at_unix_ms: number | null
  transition_active: boolean
}

export interface RelayIntegrationStatus {
  configured: boolean
  relay_enabled: boolean
  config: RelayIntegrationConfigStatus | null
  credentials: RelayIntegrationCredentialStatus | null
}

export interface RelayPriceSyncSummary {
  synced_at: string
  total_models: number
  total_channels_synced: number
  failed_channel_count: number
}

interface RelayPriceSyncPayload {
  synced_at: string
  total_models: number
  total_channels_synced: number
  failed_channels: string[]
}

export interface RelayDashboardSummary {
  total_channels: number
  enabled_channels: number
  total_downstream: number
  last_price_sync: RelayPriceSyncSummary | null
}

interface RelayDashboardPayload {
  total_channels: number
  enabled_channels: number
  total_downstream: number
  last_price_sync: RelayPriceSyncPayload | null
}

export interface RelayIntegrationOverview {
  status: RelayIntegrationStatus | null
  dashboard: RelayDashboardSummary | null
  statusError: boolean
  dashboardError: boolean
}

interface RelayApiEnvelope<T> {
  success: boolean
  data: T
}

function toBrowserSafeIntegrationStatus(status: RelayIntegrationStatus): RelayIntegrationStatus {
  return {
    configured: status.configured,
    relay_enabled: status.relay_enabled,
    config: status.config && {
      instance_id: status.config.instance_id,
      route_profile: status.config.route_profile,
      execution_mode: status.config.execution_mode,
      enabled: status.config.enabled,
      capability_version: status.config.capability_version,
      revision: status.config.revision,
      updated_at_unix_ms: status.config.updated_at_unix_ms,
    },
    credentials: status.credentials && {
      credential_revision: status.credentials.credential_revision,
      transition_expires_at_unix_ms: status.credentials.transition_expires_at_unix_ms,
      transition_active: status.credentials.transition_active,
    },
  }
}

function toBrowserSafeDashboardSummary(dashboard: RelayDashboardPayload): RelayDashboardSummary {
  const priceSync = dashboard.last_price_sync
  return {
    total_channels: dashboard.total_channels,
    enabled_channels: dashboard.enabled_channels,
    total_downstream: dashboard.total_downstream,
    last_price_sync: priceSync && {
      synced_at: priceSync.synced_at,
      total_models: priceSync.total_models,
      total_channels_synced: priceSync.total_channels_synced,
      failed_channel_count: priceSync.failed_channels.length,
    },
  }
}

export const relayIntegrationApi = {
  async getStatus(): Promise<RelayIntegrationStatus> {
    const response = await apiClient.get<RelayApiEnvelope<RelayIntegrationStatus>>(
      '/api/relay/integration-status'
    )
    return toBrowserSafeIntegrationStatus(response.data.data)
  },

  async getDashboard(): Promise<RelayDashboardSummary> {
    const response = await apiClient.get<RelayApiEnvelope<RelayDashboardPayload>>(
      '/api/relay/dashboard'
    )
    return toBrowserSafeDashboardSummary(response.data.data)
  },

  async getOverview(): Promise<RelayIntegrationOverview> {
    const [status, dashboard] = await Promise.allSettled([
      relayIntegrationApi.getStatus(),
      relayIntegrationApi.getDashboard(),
    ])

    return {
      status: status.status === 'fulfilled' ? status.value : null,
      dashboard: dashboard.status === 'fulfilled' ? dashboard.value : null,
      statusError: status.status === 'rejected',
      dashboardError: dashboard.status === 'rejected',
    }
  },
}
