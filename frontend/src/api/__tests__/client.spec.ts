import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AxiosAdapter, AxiosError, AxiosInstance, AxiosResponse, InternalAxiosRequestConfig } from 'axios'

import apiClient, { AUTH_STATE_CHANGE_EVENT } from '@/api/client'

type TestableApiClient = {
  client: AxiosInstance
}

describe('apiClient auth state change event', () => {
  beforeEach(() => {
    localStorage.clear()
    apiClient.clearAuth()
  })

  afterEach(() => {
    localStorage.clear()
    apiClient.clearAuth()
  })

  it('dispatches a same-tab auth change event when clearing auth', () => {
    const handler = vi.fn()
    window.addEventListener(AUTH_STATE_CHANGE_EVENT, handler as EventListener)

    apiClient.setToken('access-token')
    apiClient.clearAuth()

    expect(localStorage.getItem('access_token')).toBeNull()
    expect(handler).toHaveBeenCalledTimes(1)

    const event = handler.mock.calls[0][0] as CustomEvent<{ token: string | null }>
    expect(event.detail).toEqual({ token: null })

    window.removeEventListener(AUTH_STATE_CHANGE_EVENT, handler as EventListener)
  })

  it('sends auth refresh without a request body', async () => {
    const rawClient = apiClient as unknown as TestableApiClient
    const previousAdapter = rawClient.client.defaults.adapter
    const requests: InternalAxiosRequestConfig[] = []

    rawClient.client.defaults.adapter = (async (config: InternalAxiosRequestConfig) => {
      requests.push(config)
      return {
        data: { access_token: 'new-access-token' },
        status: 200,
        statusText: 'OK',
        headers: {},
        config,
      }
    }) as AxiosAdapter

    try {
      const response = await apiClient.refreshToken()

      expect(response.data.access_token).toBe('new-access-token')
      expect(requests).toHaveLength(1)
      expect(requests[0].url).toBe('/api/auth/refresh')
      expect(requests[0].method).toBe('post')
      expect(requests[0].data).toBeUndefined()
    } finally {
      rawClient.client.defaults.adapter = previousAdapter
    }
  })

  it('returns the retried response after a 401 refresh succeeds', async () => {
    const rawClient = apiClient as unknown as TestableApiClient
    const previousAdapter = rawClient.client.defaults.adapter
    const requests: InternalAxiosRequestConfig[] = []
    let protectedRequestCount = 0

    const responseFor = <T>(data: T, config: InternalAxiosRequestConfig): AxiosResponse<T> => ({
      data,
      status: 200,
      statusText: 'OK',
      headers: {},
      config,
    })

    rawClient.client.defaults.adapter = ((config: InternalAxiosRequestConfig) => {
      requests.push(config)

      if (config.url === '/api/auth/refresh') {
        return Promise.resolve(responseFor({ access_token: 'refreshed-token' }, config))
      }

      if (config.url === '/api/private/retry') {
        protectedRequestCount += 1

        if (protectedRequestCount === 1) {
          const response = {
            ...responseFor({ detail: 'Unauthorized' }, config),
            status: 401,
            statusText: 'Unauthorized',
          }
          const error = Object.assign(new Error('Unauthorized'), {
            config,
            isAxiosError: true,
            response,
          }) as AxiosError

          return Promise.reject(error)
        }

        return Promise.resolve(responseFor({ result: 'retried' }, config))
      }

      return Promise.reject(new Error(`Unexpected request: ${config.url}`))
    }) as AxiosAdapter

    try {
      apiClient.setToken('expired-token')

      const response = await apiClient.get<{ result: string }>('/api/private/retry')

      expect(response.data).toEqual({ result: 'retried' })
      expect(requests.map((request) => request.url)).toEqual([
        '/api/private/retry',
        '/api/auth/refresh',
        '/api/private/retry',
      ])
      expect(requests[2].headers.Authorization).toBe('Bearer refreshed-token')
    } finally {
      rawClient.client.defaults.adapter = previousAdapter
    }
  })
})
