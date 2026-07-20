<template>
  <PageContainer max-width="xl">
    <div class="space-y-6">
      <PageHeader
        :title="t('relayIntegration.title')"
        :description="t('relayIntegration.description')"
        :icon="GitBranch"
      >
        <template #actions>
          <RefreshButton
            :loading="loading"
            :title="t('relayIntegration.refresh')"
            @click="loadStatus"
          />
        </template>
      </PageHeader>

      <Card
        v-if="error"
        class="border-destructive/40 bg-destructive/5 p-5"
        role="alert"
      >
        <div class="flex items-start gap-3">
          <AlertCircle class="mt-0.5 h-5 w-5 shrink-0 text-destructive" />
          <div class="min-w-0">
            <h2 class="text-sm font-semibold text-foreground">
              {{ t('relayIntegration.errorTitle') }}
            </h2>
            <p class="mt-1 text-sm text-muted-foreground">
              {{ t('relayIntegration.errorDescription') }}
            </p>
          </div>
        </div>
      </Card>

      <div
        v-if="loading && !status"
        class="grid grid-cols-1 gap-4 md:grid-cols-3"
        aria-busy="true"
      >
        <Card
          v-for="index in 3"
          :key="index"
          class="space-y-3 p-5"
        >
          <Skeleton class="h-4 w-28" />
          <Skeleton class="h-7 w-36" />
          <Skeleton class="h-3 w-24" />
        </Card>
      </div>

      <template v-else-if="status">
        <section
          class="grid grid-cols-1 gap-4 md:grid-cols-3"
          :aria-label="t('relayIntegration.statusTitle')"
        >
          <Card class="p-5">
            <div class="flex items-start justify-between gap-3">
              <div>
                <p class="text-xs font-medium text-muted-foreground">
                  {{ t('relayIntegration.configuration') }}
                </p>
                <p class="mt-2 text-lg font-semibold">
                  {{ configuredLabel }}
                </p>
              </div>
              <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg border border-border/60 bg-muted/40">
                <Settings2 class="h-5 w-5 text-muted-foreground" />
              </div>
            </div>
            <p class="mt-3 text-xs text-muted-foreground">
              {{ status.config?.instance_id || t('relayIntegration.unavailable') }}
            </p>
          </Card>

          <Card class="p-5">
            <div class="flex items-start justify-between gap-3">
              <div>
                <p class="text-xs font-medium text-muted-foreground">
                  {{ t('relayIntegration.localRuntime') }}
                </p>
                <p class="mt-2 text-lg font-semibold">
                  {{ relayEnabledLabel }}
                </p>
              </div>
              <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg border border-border/60 bg-muted/40">
                <RadioTower class="h-5 w-5 text-muted-foreground" />
              </div>
            </div>
            <p class="mt-3 text-xs text-muted-foreground">
              {{ t('relayIntegration.localRuntimeHint') }}
            </p>
          </Card>

          <Card class="p-5">
            <div class="flex items-start justify-between gap-3">
              <div>
                <p class="text-xs font-medium text-muted-foreground">
                  {{ t('relayIntegration.operationStatus') }}
                </p>
                <div class="mt-2">
                  <Badge :variant="operationStatus.variant">
                    {{ operationStatus.label }}
                  </Badge>
                </div>
              </div>
              <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg border border-border/60 bg-muted/40">
                <CircleCheck class="h-5 w-5 text-muted-foreground" />
              </div>
            </div>
            <p class="mt-3 text-xs text-muted-foreground">
              {{ executionModeLabel }}
            </p>
          </Card>
        </section>

        <section :aria-labelledby="dashboardSectionId">
          <div class="mb-3">
            <h2
              :id="dashboardSectionId"
              class="text-sm font-semibold"
            >
              {{ t('relayIntegration.dashboardTitle') }}
            </h2>
            <p class="mt-1 text-xs text-muted-foreground">
              {{ t('relayIntegration.dashboardDescription') }}
            </p>
          </div>

          <div
            v-if="dashboardLoading"
            class="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4"
            aria-busy="true"
          >
            <Card
              v-for="index in 4"
              :key="index"
              class="space-y-3 p-5"
            >
              <Skeleton class="h-4 w-24" />
              <Skeleton class="h-7 w-20" />
              <Skeleton class="h-3 w-32" />
            </Card>
          </div>

          <Card
            v-else-if="dashboardError"
            class="border-border/60 bg-muted/30 p-5"
            role="status"
          >
            <div class="flex items-start gap-3">
              <AlertCircle class="mt-0.5 h-5 w-5 shrink-0 text-muted-foreground" />
              <div>
                <h3 class="text-sm font-semibold">
                  {{ t('relayIntegration.dashboardUnavailableTitle') }}
                </h3>
                <p class="mt-1 text-sm text-muted-foreground">
                  {{ t('relayIntegration.dashboardUnavailableDescription') }}
                </p>
              </div>
            </div>
          </Card>

          <div
            v-else-if="dashboard"
            class="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4"
          >
            <Card class="p-5">
              <p class="text-xs font-medium text-muted-foreground">
                {{ t('relayIntegration.channels') }}
              </p>
              <p class="mt-2 text-lg font-semibold tabular-nums">
                {{ enabledChannelSummary }}
              </p>
              <p class="mt-3 text-xs text-muted-foreground">
                {{ t('relayIntegration.enabledChannels') }}
              </p>
            </Card>

            <Card class="p-5">
              <p class="text-xs font-medium text-muted-foreground">
                {{ t('relayIntegration.downstreamInstances') }}
              </p>
              <p class="mt-2 text-lg font-semibold tabular-nums">
                {{ formatNumber(dashboard.total_downstream) }}
              </p>
              <p class="mt-3 text-xs text-muted-foreground">
                {{ t('relayIntegration.currentDownstream') }}
              </p>
            </Card>

            <Card class="p-5">
              <p class="text-xs font-medium text-muted-foreground">
                {{ t('relayIntegration.lastPriceSync') }}
              </p>
              <p class="mt-2 text-lg font-semibold">
                {{ lastPriceSyncLabel }}
              </p>
              <p class="mt-3 text-xs text-muted-foreground">
                {{ priceSyncResultLabel }}
              </p>
            </Card>

            <Card class="p-5">
              <p class="text-xs font-medium text-muted-foreground">
                {{ t('relayIntegration.priceSyncFailures') }}
              </p>
              <p class="mt-2 text-lg font-semibold tabular-nums">
                {{ priceSyncFailureCount }}
              </p>
              <p class="mt-3 text-xs text-muted-foreground">
                {{ t('relayIntegration.priceSyncFailureHint') }}
              </p>
            </Card>
          </div>
        </section>

        <section :aria-labelledby="configurationSectionId">
          <div class="mb-3 flex items-end justify-between gap-4">
            <div>
              <h2
                :id="configurationSectionId"
                class="text-sm font-semibold"
              >
                {{ t('relayIntegration.configTitle') }}
              </h2>
              <p class="mt-1 text-xs text-muted-foreground">
                {{ t('relayIntegration.configDescription') }}
              </p>
            </div>
          </div>

          <Card class="overflow-hidden">
            <dl class="grid grid-cols-1 divide-y divide-border/60 sm:grid-cols-2 sm:divide-x sm:divide-y-0">
              <StatusField
                :label="t('relayIntegration.instanceId')"
                :value="status.config?.instance_id || t('relayIntegration.unavailable')"
              />
              <StatusField
                :label="t('relayIntegration.routeProfile')"
                :value="status.config?.route_profile || t('relayIntegration.unavailable')"
              />
              <StatusField
                :label="t('relayIntegration.executionMode')"
                :value="executionModeLabel"
              />
              <StatusField
                :label="t('relayIntegration.integrationState')"
                :value="integrationEnabledLabel"
              />
              <StatusField
                :label="t('relayIntegration.capabilityVersion')"
                :value="status.config?.capability_version || t('relayIntegration.unavailable')"
              />
              <StatusField
                :label="t('relayIntegration.configRevision')"
                :value="formatNumber(status.config?.revision)"
              />
              <StatusField
                class="sm:col-span-2 sm:border-t"
                :label="t('relayIntegration.lastUpdated')"
                :value="formatUnixMs(status.config?.updated_at_unix_ms)"
              />
            </dl>
          </Card>
        </section>

        <section :aria-labelledby="credentialsSectionId">
          <div class="mb-3">
            <h2
              :id="credentialsSectionId"
              class="text-sm font-semibold"
            >
              {{ t('relayIntegration.credentialsTitle') }}
            </h2>
            <p class="mt-1 text-xs text-muted-foreground">
              {{ t('relayIntegration.credentialsDescription') }}
            </p>
          </div>

          <Card class="overflow-hidden">
            <dl class="grid grid-cols-1 divide-y divide-border/60 sm:grid-cols-3 sm:divide-x sm:divide-y-0">
              <StatusField
                :label="t('relayIntegration.credentialRevision')"
                :value="formatNumber(status.credentials?.credential_revision)"
              />
              <StatusField
                :label="t('relayIntegration.credentialTransition')"
                :value="credentialTransitionLabel"
              />
              <StatusField
                :label="t('relayIntegration.transitionExpires')"
                :value="formatUnixMs(status.credentials?.transition_expires_at_unix_ms)"
              />
            </dl>
          </Card>
        </section>

        <section :aria-labelledby="relatedSectionId">
          <div class="mb-3">
            <h2
              :id="relatedSectionId"
              class="text-sm font-semibold"
            >
              {{ t('relayIntegration.relatedTitle') }}
            </h2>
          </div>

          <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
            <RouterLink
              v-for="item in relatedViews"
              :key="item.to"
              :to="item.to"
              class="group flex min-h-16 items-center gap-3 rounded-lg border border-border/60 bg-card px-4 py-3 text-sm font-medium transition-colors hover:border-primary/50 hover:bg-primary/5 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
            >
              <component
                :is="item.icon"
                class="h-4 w-4 shrink-0 text-muted-foreground transition-colors group-hover:text-primary"
              />
              <span>{{ item.label }}</span>
            </RouterLink>
          </div>
        </section>
      </template>
    </div>
  </PageContainer>
</template>

<script setup lang="ts">
import { computed, defineComponent, h, onBeforeUnmount, onMounted, ref, type Component } from 'vue'
import { RouterLink } from 'vue-router'
import {
  AlertCircle,
  BarChart3,
  CircleCheck,
  Database,
  GitBranch,
  RadioTower,
  Route,
  Server,
  Settings2,
} from 'lucide-vue-next'
import {
  relayIntegrationApi,
  type RelayDashboardSummary,
  type RelayIntegrationStatus,
} from '@/api/relay-integration'
import { PageContainer, PageHeader } from '@/components/layout'
import { Badge, Card, RefreshButton, Skeleton } from '@/components/ui'
import { useI18n } from '@/i18n'

type BadgeVariant = 'success' | 'warning' | 'destructive' | 'outline'

const StatusField = defineComponent({
  name: 'RelayIntegrationStatusField',
  props: {
    label: { type: String, required: true },
    value: { type: String, required: true },
  },
  setup(props) {
    return () => h('div', { class: 'min-w-0 px-4 py-4' }, [
      h('dt', { class: 'text-xs text-muted-foreground' }, props.label),
      h('dd', { class: 'mt-1 break-words text-sm font-medium text-foreground' }, props.value),
    ])
  },
})

const { locale, t } = useI18n()
const status = ref<RelayIntegrationStatus | null>(null)
const dashboard = ref<RelayDashboardSummary | null>(null)
const loading = ref(true)
const error = ref(false)
const dashboardLoading = ref(true)
const dashboardError = ref(false)
const dashboardSectionId = 'relay-integration-dashboard'
const configurationSectionId = 'relay-integration-configuration'
const credentialsSectionId = 'relay-integration-credentials'
const relatedSectionId = 'relay-integration-related'
let requestId = 0

const configuredLabel = computed(() => (
  status.value?.configured
    ? t('relayIntegration.configured')
    : t('relayIntegration.notConfigured')
))

const relayEnabledLabel = computed(() => (
  status.value?.relay_enabled
    ? t('relayIntegration.relayEnabled')
    : t('relayIntegration.relayDisabled')
))

const integrationEnabledLabel = computed(() => {
  if (!status.value?.config) return t('relayIntegration.unavailable')
  return status.value.config.enabled
    ? t('relayIntegration.integrationEnabled')
    : t('relayIntegration.integrationDisabled')
})

const executionModeLabel = computed(() => {
  const executionMode = status.value?.config?.execution_mode
  if (!executionMode) return t('relayIntegration.unavailable')
  if (executionMode === 'direct_channel') return t('relayIntegration.directChannel')
  return t('relayIntegration.reservedMode', { mode: executionMode })
})

const credentialTransitionLabel = computed(() => {
  if (!status.value?.credentials) return t('relayIntegration.unavailable')
  return status.value.credentials.transition_active
    ? t('relayIntegration.transitionActive')
    : t('relayIntegration.transitionInactive')
})

const enabledChannelSummary = computed(() => {
  if (!dashboard.value) return t('relayIntegration.unavailable')
  return `${formatNumber(dashboard.value.enabled_channels)} / ${formatNumber(dashboard.value.total_channels)}`
})

const lastPriceSyncLabel = computed(() => (
  formatIsoTimestamp(dashboard.value?.last_price_sync?.synced_at)
))

const priceSyncResultLabel = computed(() => {
  const summary = dashboard.value?.last_price_sync
  if (!summary) return t('relayIntegration.unavailable')
  return t('relayIntegration.priceSyncResult', {
    models: formatNumber(summary.total_models),
    channels: formatNumber(summary.total_channels_synced),
  })
})

const priceSyncFailureCount = computed(() => {
  const count = dashboard.value?.last_price_sync?.failed_channel_count
  return formatNumber(count)
})

const operationStatus = computed<{ label: string, variant: BadgeVariant }>(() => {
  const current = status.value
  if (!current?.relay_enabled) {
    return { label: t('relayIntegration.relayDisabled'), variant: 'outline' }
  }
  if (!current.configured || !current.config) {
    return { label: t('relayIntegration.notConfigured'), variant: 'outline' }
  }
  if (!current.config.enabled) {
    return { label: t('relayIntegration.integrationDisabled'), variant: 'warning' }
  }
  if (current.config.execution_mode !== 'direct_channel') {
    return { label: t('relayIntegration.reservedModeStatus'), variant: 'warning' }
  }
  return { label: t('relayIntegration.operationActive'), variant: 'success' }
})

const relatedViews = computed<Array<{ to: string, label: string, icon: Component }>>(() => [
  { to: '/admin/providers', label: t('relayIntegration.providers'), icon: Server },
  { to: '/admin/pool', label: t('relayIntegration.pool'), icon: Database },
  { to: '/admin/routing', label: t('relayIntegration.routing'), icon: Route },
  { to: '/admin/usage', label: t('relayIntegration.usage'), icon: BarChart3 },
])

function formatNumber(value: number | null | undefined): string {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return t('relayIntegration.unavailable')
  }
  return new Intl.NumberFormat(locale.value).format(value)
}

function formatUnixMs(value: number | null | undefined): string {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) {
    return t('relayIntegration.unavailable')
  }
  return new Intl.DateTimeFormat(locale.value, {
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(new Date(value))
}

function formatIsoTimestamp(value: string | null | undefined): string {
  if (!value) return t('relayIntegration.unavailable')
  const timestamp = new Date(value)
  if (Number.isNaN(timestamp.getTime())) return t('relayIntegration.unavailable')
  return new Intl.DateTimeFormat(locale.value, {
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(timestamp)
}

async function loadStatus(): Promise<void> {
  const currentRequestId = ++requestId
  loading.value = true
  error.value = false
  dashboardLoading.value = true
  dashboardError.value = false

  try {
    const overview = await relayIntegrationApi.getOverview()
    if (currentRequestId !== requestId) return
    status.value = overview.status
    dashboard.value = overview.dashboard
    error.value = overview.statusError
    dashboardError.value = overview.dashboardError
  } catch {
    if (currentRequestId !== requestId) return
    error.value = true
    dashboardError.value = true
  } finally {
    if (currentRequestId === requestId) {
      loading.value = false
      dashboardLoading.value = false
    }
  }
}

onMounted(() => {
  void loadStatus()
})

onBeforeUnmount(() => {
  requestId += 1
})
</script>
