import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

import { useIdentityStore } from './identity-store'
import { memoryStorage } from './memory-storage.fixture'

const server = setupServer()
const apiUrl = 'http://exchange.test'
const address = `0x${'ab'.repeat(20)}`
const STORAGE_KEY = 'kyros:exchange-identity'
const stored = { address, username: 'jet', color: '#fff', emoji: '🦊' }

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
    isVerifying: false,
    error: null,
  })
})
afterEach(() => {
  server.resetHandlers()
  vi.restoreAllMocks()
})
afterAll(() => server.close())

describe('identity store', () => {
  it('creates a guest account through the exchange and persists it', async () => {
    let username: unknown
    server.use(
      http.post(`${apiUrl}/api/accounts`, async ({ request }) => {
        username = ((await request.json()) as { username: unknown }).username
        return HttpResponse.json({ ...stored, usdc_balance: 1_000_000 }, { status: 201 })
      })
    )
    await useIdentityStore.getState().createGuest(apiUrl)
    expect(username).toMatch(/^guest-[a-z0-9]{6}$/)
    expect(useIdentityStore.getState().identity).toEqual(stored)
    expect(useIdentityStore.getState().balanceTenths).toBe(1_000_000)
    expect(JSON.parse(window.localStorage.getItem(STORAGE_KEY) ?? '')).toEqual(stored)
  })

  it('sends one request when a guest is requested twice before the first answers', async () => {
    let posts = 0
    server.use(
      http.post(`${apiUrl}/api/accounts`, () => {
        posts += 1
        return HttpResponse.json({ ...stored, usdc_balance: 1_000_000 }, { status: 201 })
      })
    )
    const first = useIdentityStore.getState().createGuest(apiUrl)
    const second = useIdentityStore.getState().createGuest(apiUrl)
    await Promise.all([first, second])
    expect(posts).toBe(1)
  })

  it('reports a failed guest creation and stays signed out', async () => {
    server.use(
      http.post(`${apiUrl}/api/accounts`, () =>
        HttpResponse.json({ error: 'internal', message: 'boom' }, { status: 500 })
      )
    )
    await useIdentityStore.getState().createGuest(apiUrl)
    expect(useIdentityStore.getState().identity).toBeNull()
    expect(useIdentityStore.getState().isCreating).toBe(false)
    expect(useIdentityStore.getState().error).toBe('Could not reach the exchange.')
  })

  it('drops a stored identity the exchange no longer knows', async () => {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ error: 'unknown_account', message: 'gone' }, { status: 404 })
      )
    )
    await useIdentityStore.getState().verify(apiUrl)
    expect(useIdentityStore.getState().identity).toBeNull()
    expect(useIdentityStore.getState().verified).toBe(true)
    expect(window.localStorage.getItem(STORAGE_KEY)).toBeNull()
  })

  it('keeps a stored identity when the exchange fails rather than forgets it', async () => {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ error: 'internal', message: 'boom' }, { status: 500 })
      )
    )
    await useIdentityStore.getState().verify(apiUrl)
    expect(useIdentityStore.getState().identity).toEqual(stored)
    expect(JSON.parse(window.localStorage.getItem(STORAGE_KEY) ?? '')).toEqual(stored)
  })

  it('sends one request when verify is called twice before the first answers', async () => {
    let gets = 0
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () => {
        gets += 1
        return HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 })
      })
    )
    const first = useIdentityStore.getState().verify(apiUrl)
    const second = useIdentityStore.getState().verify(apiUrl)
    await Promise.all([first, second])
    expect(gets).toBe(1)
  })

  it('drops an answer about an account the user has since replaced', async () => {
    const next = {
      address: `0x${'cd'.repeat(20)}`,
      username: 'guest-abc123',
      color: '#000',
      emoji: '🦉',
    }
    let release: ((response: Response) => void) | undefined
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored))
    useIdentityStore.setState({ identity: stored, verified: true })
    server.use(
      http.get(
        `${apiUrl}/api/accounts/${address}`,
        () => new Promise<Response>(resolve => (release = resolve))
      ),
      http.get(`${apiUrl}/api/accounts/${next.address}`, () =>
        HttpResponse.json({ address: next.address, usdc_balance: 1_000_000, locked: 0 })
      )
    )
    const stale = useIdentityStore.getState().verify(apiUrl)
    await vi.waitFor(() => expect(release).toBeDefined())
    // "New paper account" while the old account's check is still on its way.
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next))
    useIdentityStore.setState({ identity: next })
    release?.(HttpResponse.json({ error: 'unknown_account', message: 'gone' }, { status: 404 }))
    await stale

    expect(useIdentityStore.getState().identity).toEqual(next)
    expect(JSON.parse(window.localStorage.getItem(STORAGE_KEY) ?? '')).toEqual(next)
    expect(useIdentityStore.getState().isVerifying).toBe(false)
    // The check ran again for the account that is stored now.
    expect(useIdentityStore.getState().balanceTenths).toBe(1_000_000)
  })

  it('keeps a held account, and still checks it, when storage cannot be read', async () => {
    vi.spyOn(window.localStorage, 'getItem').mockImplementation(() => {
      throw new Error('storage denied')
    })
    useIdentityStore.setState({ identity: stored, verified: true })
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 })
      )
    )
    await useIdentityStore.getState().verify(apiUrl)
    expect(useIdentityStore.getState().identity).toEqual(stored)
    expect(useIdentityStore.getState().balanceTenths).toBe(987_650)
  })

  it('drops a held account the exchange forgot, when storage cannot be read', async () => {
    vi.spyOn(window.localStorage, 'getItem').mockImplementation(() => {
      throw new Error('storage denied')
    })
    useIdentityStore.setState({ identity: stored, verified: true })
    let asked = 0
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () => {
        asked += 1
        return HttpResponse.json({ error: 'unknown_account', message: 'gone' }, { status: 404 })
      })
    )
    await useIdentityStore.getState().verify(apiUrl)
    // The account is dropped because the exchange said so, not because
    // storage was unreadable: the question must have been asked.
    expect(asked).toBe(1)
    expect(useIdentityStore.getState().identity).toBeNull()
    expect(useIdentityStore.getState().verified).toBe(true)
    expect(useIdentityStore.getState().isVerifying).toBe(false)
  })

  it('keeps a stored identity the exchange confirms, with its balance', async () => {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(stored))
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
