import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const source = readFileSync(resolve(__dirname, '../ProviderFormDialog.vue'), 'utf-8')

describe('ProviderFormDialog upstream policy', () => {
  it('renders the upstream policy section', () => {
    expect(source).toContain('data-testid="upstream-policy-setting"')
    expect(source).toContain('upstream_policy')
  })

  it('binds mode, attempts, error passthrough and empty-response fields', () => {
    expect(source).toContain('form.upstream_policy_mode')
    expect(source).toContain('form.upstream_policy_max_attempts')
    expect(source).toContain('form.upstream_policy_passthrough_errors')
    expect(source).toContain('form.upstream_policy_empty_detect')
    expect(source).toContain('form.upstream_policy_empty_max_attempts')
    expect(source).toContain('form.upstream_policy_empty_on_exhausted')
  })

  it('resets and loads upstream policy state', () => {
    expect(source).toMatch(/resetForm[\s\S]*upstream_policy_mode/)
    expect(source).toMatch(/loadProviderData[\s\S]*upstream_policy_mode/)
  })

  it('submits the upstream_policy payload and supports full passthrough', () => {
    expect(source).toContain('upstream_policy: buildUpstreamPolicyPayload()')
    expect(source).toContain("'full_passthrough'")
    expect(source).toContain('empty_response')
  })
})
