import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

import { IdentityGate } from './identity-gate'
import { useIdentityStore } from './identity-store'
import { memoryStorage } from './memory-storage.fixture'

const server = setupServer()
const apiUrl = 'http://exchange.test'
const address = `0x${'cd'.repeat(20)}`
const account = { address, username: 'guest-a1b2c3', color: '#fff', emoji: '🦊' }

function created() {
  return HttpResponse.json({ ...account, usdc_balance: 1_000_000 }, { status: 201 })
}

function failed() {
  return HttpResponse.json({ error: 'internal', message: 'boom' }, { status: 500 })
}

function renderGate() {
  return render(
    <IdentityGate apiUrl={apiUrl}>
      <p>Trading floor</p>
    </IdentityGate>
  )
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

describe('IdentityGate', () => {
  it('opens a guest account silently and renders the app', async () => {
    server.use(http.post(`${apiUrl}/api/accounts`, created))
    renderGate()
    expect(screen.queryByRole('textbox')).toBeNull()

    expect(await screen.findByText('Trading floor')).toBeInTheDocument()
    expect(screen.queryByRole('textbox')).toBeNull()
  })

  it('offers a retry when the exchange cannot open an account', async () => {
    const user = userEvent.setup()
    server.use(http.post(`${apiUrl}/api/accounts`, failed))
    renderGate()

    expect(await screen.findByText('Could not reach the exchange.')).toBeInTheDocument()
    const retry = screen.getByRole('button', { name: 'Try again' })

    server.use(http.post(`${apiUrl}/api/accounts`, created))
    await user.click(retry)

    expect(await screen.findByText('Trading floor')).toBeInTheDocument()
  })

  it('keeps a stored account the exchange still knows without opening another', async () => {
    let posts = 0
    window.localStorage.setItem('kyros:exchange-identity', JSON.stringify(account))
    server.use(
      http.get(`${apiUrl}/api/accounts/${address}`, () =>
        HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 })
      ),
      http.post(`${apiUrl}/api/accounts`, () => {
        posts += 1
        return created()
      })
    )
    renderGate()

    expect(await screen.findByText('Trading floor')).toBeInTheDocument()
    expect(posts).toBe(0)
  })

  it('has an accessible waiting screen', async () => {
    // Never answers, so the screen stays on the waiting state for the audit.
    server.use(http.post(`${apiUrl}/api/accounts`, () => new Promise<never>(() => undefined)))
    const { container } = renderGate()

    expect(screen.getByRole('heading', { name: 'Welcome to Kyros' })).toBeInTheDocument()
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent(/paper account/))
    await expectNoA11yViolations(container)
  })

  describe('while an account is held', () => {
    const REVERIFY_MS = 60_000
    let gets = 0
    let posts = 0

    function hold(getAnswer: () => Response) {
      window.localStorage.setItem('kyros:exchange-identity', JSON.stringify(account))
      useIdentityStore.setState({ identity: account, verified: true })
      server.use(
        http.get(`${apiUrl}/api/accounts/${address}`, () => {
          gets += 1
          return getAnswer()
        }),
        http.post(`${apiUrl}/api/accounts`, () => {
          posts += 1
          return HttpResponse.json(
            { ...account, address: `0x${'ef'.repeat(20)}`, usdc_balance: 1_000_000 },
            { status: 201 }
          )
        })
      )
    }

    beforeEach(() => {
      gets = 0
      posts = 0
      // Only the interval is faked, so MSW's own timers keep running. Testing Library's waitFor
      // polls on an interval, so the waits below on counters use vi.waitFor instead.
      vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    })
    afterEach(() => vi.useRealTimers())

    it('checks the account with the exchange every minute', async () => {
      hold(() => HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 }))
      renderGate()
      expect(screen.getByText('Trading floor')).toBeInTheDocument()
      expect(gets).toBe(0)

      act(() => vi.advanceTimersByTime(REVERIFY_MS))

      await vi.waitFor(() => expect(gets).toBe(1))
      expect(screen.getByText('Trading floor')).toBeInTheDocument()
    })

    it('checks the account again when the window regains focus', async () => {
      hold(() => HttpResponse.json({ address, usdc_balance: 987_650, locked: 0 }))
      renderGate()

      act(() => void window.dispatchEvent(new Event('focus')))

      await vi.waitFor(() => expect(gets).toBe(1))
    })

    it('opens a new guest when the exchange restarted and forgot the account', async () => {
      hold(() => HttpResponse.json({ error: 'unknown_account', message: 'gone' }, { status: 404 }))
      renderGate()

      act(() => vi.advanceTimersByTime(REVERIFY_MS))

      await vi.waitFor(() => expect(posts).toBe(1))
      expect(await screen.findByText('Trading floor')).toBeInTheDocument()
      expect(useIdentityStore.getState().identity?.address).toBe(`0x${'ef'.repeat(20)}`)
    })

    it('keeps the account when the exchange fails to answer', async () => {
      hold(() => HttpResponse.json({ error: 'internal', message: 'boom' }, { status: 500 }))
      renderGate()

      act(() => vi.advanceTimersByTime(REVERIFY_MS))

      await vi.waitFor(() => expect(gets).toBe(1))
      await vi.waitFor(() => expect(useIdentityStore.getState().isVerifying).toBe(false))
      expect(posts).toBe(0)
      expect(useIdentityStore.getState().identity).toEqual(account)
      expect(screen.getByText('Trading floor')).toBeInTheDocument()
    })
  })
})
