import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, type App } from 'vue'

import RoutingProfiles from '../RoutingProfiles.vue'
import { createEmptyModelPolicy, createEmptyRoutingGroupConfig } from '@/features/routing/utils/routingPolicy'
import type { RoutingGroupRecord } from '@/api/routing-profiles'

const routeMock = vi.hoisted(() => ({
  name: 'RoutingProfileDetail',
  params: { groupId: 'routing-profile-1' },
}))
const routerReplaceMock = vi.hoisted(() => vi.fn())
const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  error: vi.fn(),
}))
const routingProfileApiMocks = vi.hoisted(() => ({
  createRoutingGroup: vi.fn(),
  deleteRoutingGroup: vi.fn(),
  listRoutingGroups: vi.fn(),
  updateRoutingGroup: vi.fn(),
}))
const globalModelApiMocks = vi.hoisted(() => ({
  getGlobalModels: vi.fn(),
}))

vi.mock('vue-router', async (importOriginal) => {
  const actual = await importOriginal<typeof import('vue-router')>()
  return {
    ...actual,
    useRoute: () => routeMock,
    useRouter: () => ({
      push: vi.fn(),
      replace: routerReplaceMock,
    }),
  }
})

vi.mock('@/api/routing-profiles', () => routingProfileApiMocks)

vi.mock('@/api/global-models', () => globalModelApiMocks)

vi.mock('@/composables/useToast', () => ({
  useToast: () => toastMocks,
}))

vi.mock('@/components/layout', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    PageContainer: defineComponent({
      name: 'PageContainerStub',
      setup(_props, { attrs, slots }) {
        return () => h('div', attrs, slots.default?.())
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
    Button: passthrough('ButtonStub', 'button'),
    Card: passthrough('CardStub'),
    Input: passthrough('InputStub', 'input'),
    Table: passthrough('TableStub', 'table'),
    TableBody: passthrough('TableBodyStub', 'tbody'),
    TableCard: passthrough('TableCardStub'),
    TableCell: passthrough('TableCellStub', 'td'),
    TableHead: passthrough('TableHeadStub', 'th'),
    TableHeader: passthrough('TableHeaderStub', 'thead'),
    TableRow: passthrough('TableRowStub', 'tr'),
  }
})

vi.mock('@/components/ui/dropdown-menu', async () => {
  const { defineComponent, h } = await import('vue')
  const passthrough = (name: string) => defineComponent({
    name,
    setup(_props, { attrs, slots }) {
      return () => h('div', attrs, slots.default?.())
    },
  })

  return {
    DropdownMenu: passthrough('DropdownMenuStub'),
    DropdownMenuContent: passthrough('DropdownMenuContentStub'),
    DropdownMenuItem: passthrough('DropdownMenuItemStub'),
    DropdownMenuTrigger: passthrough('DropdownMenuTriggerStub'),
  }
})

vi.mock('@/components/common', async () => {
  const { defineComponent } = await import('vue')
  return {
    AlertDialog: defineComponent({
      name: 'AlertDialogStub',
      setup: () => () => null,
    }),
  }
})

vi.mock('@/features/routing/components', () => ({
  RoutingPriorityPolicyEditor: defineComponent({
    name: 'RoutingPriorityPolicyEditorStub',
    props: {
      priorityMode: { type: String, default: '' },
      schedulingMode: { type: String, default: '' },
    },
    emits: ['update:priority-mode', 'update:scheduling-mode'],
    setup(props, { emit }) {
      return () => h('div', [
        h('output', { 'data-testid': 'priority-mode' }, props.priorityMode),
        h('output', { 'data-testid': 'scheduling-mode' }, props.schedulingMode),
        h('button', {
          'data-testid': 'set-global-key-priority',
          onClick: () => emit('update:priority-mode', 'global_key'),
        }),
        h('button', {
          'data-testid': 'set-fixed-order-scheduling',
          onClick: () => emit('update:scheduling-mode', 'fixed_order'),
        }),
      ])
    },
  }),
}))

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

const existingGroup: RoutingGroupRecord = {
  id: 'routing-profile-1',
  name: 'Existing routing profile',
  description: null,
  enabled: false,
  is_system_default: false,
  config_json: createEmptyRoutingGroupConfig(),
  version: 1,
  created_at: 1,
  updated_at: 1,
}

function mountRoutingProfiles(): HTMLElement {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(RoutingProfiles)
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

beforeEach(() => {
  routeMock.name = 'RoutingProfileDetail'
  routeMock.params = { groupId: existingGroup.id }
  routerReplaceMock.mockReset()
  toastMocks.success.mockReset()
  toastMocks.error.mockReset()
  routingProfileApiMocks.createRoutingGroup.mockReset()
  routingProfileApiMocks.deleteRoutingGroup.mockReset()
  routingProfileApiMocks.listRoutingGroups.mockResolvedValue({
    items: [existingGroup],
    total: 1,
  })
  routingProfileApiMocks.updateRoutingGroup.mockResolvedValue({
    ...existingGroup,
    enabled: true,
  })
  globalModelApiMocks.getGlobalModels.mockReset()
  globalModelApiMocks.getGlobalModels.mockResolvedValue({ models: [] })
})

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
  document.body.innerHTML = ''
})

describe('RoutingProfiles', () => {
  it('updates an existing profile with its saved ID', async () => {
    const root = mountRoutingProfiles()
    await settle()

    const enableButton = Array.from(root.querySelectorAll('button'))
      .find(button => button.textContent?.includes('启用'))
    const saveButton = root.querySelector<HTMLButtonElement>('button[title="保存"]')

    expect(enableButton).toBeDefined()
    expect(saveButton).toBeDefined()

    enableButton?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await nextTick()
    saveButton?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await settle()

    expect(routingProfileApiMocks.updateRoutingGroup).toHaveBeenCalledWith(
      existingGroup.id,
      expect.objectContaining({ enabled: true }),
    )
    expect(routingProfileApiMocks.createRoutingGroup).not.toHaveBeenCalled()
    expect(routerReplaceMock).not.toHaveBeenCalled()
  })

  it('applies policy-editor mode events to the selected per-model draft', async () => {
    routingProfileApiMocks.listRoutingGroups.mockResolvedValue({
      items: [{
        ...existingGroup,
        config_json: {
          ...createEmptyRoutingGroupConfig(),
          allowed_models: ['model-a'],
          model_policies: [createEmptyModelPolicy('model-a')],
        },
      }],
      total: 1,
    })
    globalModelApiMocks.getGlobalModels.mockResolvedValue({
      models: [{ name: 'model-a', display_name: 'Model A' }],
    })

    const root = mountRoutingProfiles()
    await settle()

    const configuredModelsButton = Array.from(root.querySelectorAll('button'))
      .find(button => button.textContent?.includes('已配置'))
    configuredModelsButton?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await nextTick()

    const priorityMode = root.querySelector<HTMLOutputElement>('[data-testid="priority-mode"]')
    const schedulingMode = root.querySelector<HTMLOutputElement>('[data-testid="scheduling-mode"]')
    const setGlobalKeyPriority = root.querySelector<HTMLButtonElement>('[data-testid="set-global-key-priority"]')
    const setFixedOrderScheduling = root.querySelector<HTMLButtonElement>('[data-testid="set-fixed-order-scheduling"]')

    expect(priorityMode?.textContent).toBe('provider')
    expect(schedulingMode?.textContent).toBe('cache_affinity')

    setGlobalKeyPriority?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    setFixedOrderScheduling?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await nextTick()

    expect(priorityMode?.textContent).toBe('global_key')
    expect(schedulingMode?.textContent).toBe('fixed_order')
  })
})
