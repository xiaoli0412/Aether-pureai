import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, type App } from 'vue'

import RelayIntegrationStatus from '../RelayIntegrationStatus.vue'

const relayIntegrationApiMocks = vi.hoisted(() => ({
  getOverview: vi.fn(),
}))

vi.mock('@/api/relay-integration', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/relay-integration')>()
  return {
    ...actual,
    relayIntegrationApi: relayIntegrationApiMocks,
  }
})

vi.mock('vue-router', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    RouterLink: defineComponent({
      name: 'RouterLinkStub',
      setup(_props, { attrs, slots }) {
        return () => h('a', attrs, slots.default?.())
      },
    }),
  }
})

vi.mock('@/components/layout', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    PageContainer: defineComponent({
      name: 'PageContainerStub',
      setup(_props, { attrs, slots }) {
        return () => h('main', attrs, slots.default?.())
      },
    }),
    PageHeader: defineComponent({
      name: 'PageHeaderStub',
      setup(_props, { attrs, slots }) {
        return () => h('header', attrs, [slots.default?.(), slots.actions?.()])
      },
    }),
  }
})

vi.mock('@/components/ui', async () => {
  const { defineComponent, h } = await import('vue')
  const passthrough = (name: string, tag = 'div') => defineComponent({
    name,
    setup(_props, { attrs, slots }) {
      return () => h(tag, attrs, slots.default?.())
    },
  })

  return {
    Badge: passthrough('BadgeStub'),
    Card: passthrough('CardStub'),
    RefreshButton: defineComponent({
      name: 'RefreshButtonStub',
      emits: ['click'],
      setup(_props, { emit }) {
        return () => h('button', {
          type: 'button',
          'data-testid': 'refresh',
          onClick: () => emit('click'),
        })
      },
    }),
    Skeleton: passthrough('SkeletonStub'),
  }
})

vi.mock('@/i18n', () => ({
  setI18nLocale: () => undefined,
  useI18n: () => ({
    legacyT: (value: unknown) => String(value),
    locale: { value: 'en-US' },
    t: (key: string, params?: Record<string, string>) => (
      params
        ? `${key}:${Object.entries(params).map(([name, value]) => `${name}=${value}`).join(',')}`
        : key
    ),
  }),
}))

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

function mountRelayIntegrationStatus(): HTMLElement {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(RelayIntegrationStatus)
  app.mount(root)
  mountedApps.push({ app, root })
  return root
}

async function settle(): Promise<void> {
  for (let index = 0; index < 4; index += 1) {
    await Promise.resolve()
    await nextTick()
  }
}

const activeStatus = {
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
    transition_expires_at_unix_ms: null,
    transition_active: false,
    rotation_id: 'must-not-be-rendered',
  },
}

beforeEach(() => {
  relayIntegrationApiMocks.getOverview.mockReset()
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
  document.body.innerHTML = ''
})

describe('RelayIntegrationStatus', () => {
  it('renders an active direct-channel integration without exposing unexpected credential fields', async () => {
    relayIntegrationApiMocks.getOverview.mockResolvedValue({
      status: activeStatus,
      dashboard: {
        total_channels: 4,
        enabled_channels: 3,
        total_downstream: 2,
        last_price_sync: {
          synced_at: '2026-07-18T10:30:00Z',
          total_models: 12,
          total_channels_synced: 3,
          failed_channel_count: 1,
        },
      },
      statusError: false,
      dashboardError: false,
    })

    const root = mountRelayIntegrationStatus()
    await settle()

    expect(relayIntegrationApiMocks.getOverview).toHaveBeenCalledTimes(1)
    expect(root.textContent).toContain('relayIntegration.operationActive')
    expect(root.textContent).toContain('3 / 4')
    expect(root.textContent).not.toContain('must-not-be-rendered')
  })

  it('keeps the integration status visible when only the dashboard fetch fails and refreshes it', async () => {
    relayIntegrationApiMocks.getOverview.mockResolvedValue({
      status: activeStatus,
      dashboard: null,
      statusError: false,
      dashboardError: true,
    })

    const root = mountRelayIntegrationStatus()
    await settle()

    expect(root.textContent).toContain('relayIntegration.operationActive')
    expect(root.textContent).toContain('relayIntegration.dashboardUnavailableTitle')

    root.querySelector<HTMLButtonElement>('[data-testid="refresh"]')?.click()
    await settle()

    expect(relayIntegrationApiMocks.getOverview).toHaveBeenCalledTimes(2)
  })
})
