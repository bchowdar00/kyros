import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest'

import { useIdentityStore } from './identity-store'

const server = setupServer()
const apiUrl = 'http://exchange.test'
const address = `0x${'ab'.repeat(20)}`
const stored = { address, username: 'jet', color: '#fff', emoji: '🦊' }

// jsdom has no storage on an opaque test origin; a Map stands in for it.
function memoryStorage(): Storage {
  const entries = new Map<string, string>()
  return {
    get length() {
      return entries.size
    },
    clear: () => entries.clear(),
    getItem: key => entries.get(key) ?? null,
    key: index => [...entries.keys()][index] ?? null,
    removeItem: key => void entries.delete(key),
    setItem: (key, value) => void entries.set(key, value),
  }
}

beforeAll(() => {
  Object.defineProperty(window, 'localStorage', { configurable: true, value: memoryStorage() })
  server.listen({ onUnhandledRequest: 'error' })
})
beforeEach(() => {
  window.localStorage.clear()
  useIdentityStore.setState({
    identity: null,
    balanceTenths: null,
    verified: false,
    isCreating: false,
    error: null,
  })
})
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('identity store', () => {
  it('creates an account through the exchange and persists it', async () => {
    server.use(
      http.post(`${apiUrl}/api/accounts`, async ({ request }) => {
        expect(await request.json()).toEqual({ username: 'jet' })
        return HttpResponse.json({ ...stored, usdc_balance: 1_000_000 }, { status: 201 })
      })
    )
    await useIdentityStore.getState().createAccount(apiUrl, '  jet ')
    expect(useIdentityStore.getState().identity).toEqual(stored)
    expect(useIdentityStore.getState().balanceTenths).toBe(1_000_000)
    expect(JSON.parse(window.localStorage.getItem('tiny:exchange-identity') ?? '')).toEqual(stored)
  })

  it('drops a stored identity the exchange no longer knows', async () => {
    window.localStorage.setItem('tiny:exchange-identity', JSON.stringify(stored))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ error: 'unknown_account', message: 'gone' }, { status: 404 })
      )
    )
    await useIdentityStore.getState().verify(apiUrl)
    expect(useIdentityStore.getState().identity).toBeNull()
    expect(useIdentityStore.getState().verified).toBe(true)
    expect(window.localStorage.getItem('tiny:exchange-identity')).toBeNull()
  })

  it('keeps a stored identity the exchange confirms, with its balance', async () => {
    window.localStorage.setItem('tiny:exchange-identity', JSON.stringify(stored))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 })
      )
    )
    await useIdentityStore.getState().verify(apiUrl)
    expect(useIdentityStore.getState().identity).toEqual(stored)
    expect(useIdentityStore.getState().balanceTenths).toBe(987_650)
  })
})
