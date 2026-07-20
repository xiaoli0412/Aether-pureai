import { beforeEach, describe, expect, it, vi } from 'vitest'

const apiClientMocks = vi.hoisted(() => ({
  get: vi.fn(),
}))

vi.mock('@/api/client', () => ({
  default: apiClientMocks,
}))

import { relayIntegrationApi } from '../relay-integration'

describe('relayIntegrationApi', () => {
  beforeEach(() => {
    apiClientMocks.get.mockReset()
  })

  it('reads the redacted admin status through the normal relay endpoint only', async () => {
    apiClientMocks.get.mockResolvedValue({
      data: {
        success: true,
        data: {
          configured: true,
          relay_enabled: true,
          config: {
            instance_id: 'primary',
            route_profile: 'balanced',
            execution_mode: 'direct_channel',
            enabled: true,
            capability_version: '0.1.0',
            revision: 4,
            updated_at_unix_ms: 1_784_073_600_000,
          },
          credentials: {
            credential_revision: 2,
            rotation_id: 'rotation-id-is-not-rendered',
            transition_expires_at_unix_ms: null,
            transition_active: false,
          },
        },
      },
    })

    const status = await relayIntegrationApi.getStatus()

    expect(status).toMatchObject({
      configured: true,
      relay_enabled: true,
      config: {
        instance_id: 'primary',
        execution_mode: 'direct_channel',
      },
    })
    expect(status.credentials).not.toHaveProperty('rotation_id')

    expect(apiClientMocks.get).toHaveBeenCalledTimes(1)
    expect(apiClientMocks.get).toHaveBeenCalledWith('/api/relay/integration-status')
    expect(apiClientMocks.get.mock.calls.map(([url]) => String(url))).not.toContain(
      '/api/integrations/new-api/v1'
    )
  })

  it('keeps integration status available when the dashboard summary cannot load', async () => {
    apiClientMocks.get.mockImplementation((url: string) => {
      if (url === '/api/relay/integration-status') {
        return Promise.resolve({
          data: {
            success: true,
            data: {
              configured: false,
              relay_enabled: true,
              config: null,
              credentials: null,
            },
          },
        })
      }
      if (url === '/api/relay/dashboard') {
        return Promise.reject(new Error('dashboard unavailable'))
      }
      return Promise.reject(new Error(`unexpected URL: ${url}`))
    })

    await expect(relayIntegrationApi.getOverview()).resolves.toEqual({
      status: {
        configured: false,
        relay_enabled: true,
        config: null,
        credentials: null,
      },
      dashboard: null,
      statusError: false,
      dashboardError: true,
    })
  })

  it('projects dashboard sync results to counts instead of channel identifiers', async () => {
    apiClientMocks.get.mockResolvedValue({
      data: {
        success: true,
        data: {
          total_channels: 4,
          enabled_channels: 3,
          total_downstream: 2,
          last_price_sync: {
            synced_at: '2026-07-18T10:30:00Z',
            total_models: 12,
            total_channels_synced: 3,
            failed_channels: ['channel-private-a'],
          },
        },
      },
    })

    const dashboard = await relayIntegrationApi.getDashboard()

    expect(dashboard).toEqual({
      total_channels: 4,
      enabled_channels: 3,
      total_downstream: 2,
      last_price_sync: {
        synced_at: '2026-07-18T10:30:00Z',
        total_models: 12,
        total_channels_synced: 3,
        failed_channel_count: 1,
      },
    })
    expect(dashboard.last_price_sync).not.toHaveProperty('failed_channels')
    expect(apiClientMocks.get).toHaveBeenCalledWith('/api/relay/dashboard')
  })
})
