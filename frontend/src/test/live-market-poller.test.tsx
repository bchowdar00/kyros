import { focusManager } from '@tanstack/react-query'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { Profiler } from 'react'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

import { LiveMarketPoller } from '@/features/market-detail/live-market-poller'

const { router } = vi.hoisted(() => ({ router: { refresh: vi.fn() } }))
vi.mock('next/navigation', () => ({ useRouter: () => router }))

const server = setupServer()
const endpoint = '*/api/series/100001/active-market'
const props = { eventId: 100_002, seriesId: 100_001 }

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
beforeEach(() => {
  router.refresh.mockClear()
  focusManager.setFocused(true)
})
afterEach(() => {
  cleanup()
  focusManager.setFocused(undefined)
  server.resetHandlers()
})
afterAll(() => server.close())

describe('LiveMarketPoller', () => {
  it('polls each second without refreshing or committing React updates for the same event', async () => {
    let requests = 0
    server.use(
      http.get(endpoint, () => {
        requests += 1
        return HttpResponse.json({ eventId: props.eventId })
      })
    )
    const commits = vi.fn()
    render(
      <Profiler id="poller" onRender={commits}>
        <LiveMarketPoller {...props} />
      </Profiler>
    )
    const initialCommits = commits.mock.calls.length

    await waitFor(() => expect(requests).toBeGreaterThanOrEqual(2), { timeout: 3500 })
    expect(router.refresh).not.toHaveBeenCalled()
    expect(commits).toHaveBeenCalledTimes(initialCommits)
  })

  it('refreshes on a new event and stops refreshing once the server delivers it', async () => {
    let requests = 0
    server.use(
      http.get(endpoint, () => {
        requests += 1
        return HttpResponse.json({ eventId: 100_004 })
      })
    )
    const { rerender } = render(<LiveMarketPoller {...props} />)
    await waitFor(() => expect(router.refresh).toHaveBeenCalledOnce(), { timeout: 2500 })

    rerender(<LiveMarketPoller {...props} eventId={100_004} />)
    await waitFor(() => expect(requests).toBeGreaterThanOrEqual(2), { timeout: 2500 })
    expect(router.refresh).toHaveBeenCalledOnce()
  })

  it('retains the current market on failure and resumes following when checks recover', async () => {
    server.use(http.get(endpoint, () => new HttpResponse(null, { status: 503 })))
    render(<LiveMarketPoller {...props} />)
    expect(await screen.findByRole('status', {}, { timeout: 2500 })).toHaveTextContent('Retrying')
    expect(router.refresh).not.toHaveBeenCalled()

    server.use(http.get(endpoint, () => HttpResponse.json({ eventId: 100_004 })))
    await waitFor(() => expect(router.refresh).toHaveBeenCalledOnce(), { timeout: 2500 })
    expect(screen.queryByRole('status')).not.toBeInTheDocument()
  })

  it('pauses in the background and checks immediately on returning', async () => {
    let requests = 0
    server.use(
      http.get(endpoint, () => {
        requests += 1
        return HttpResponse.json({ eventId: props.eventId })
      })
    )
    focusManager.setFocused(false)
    render(<LiveMarketPoller {...props} />)
    await act(() => new Promise(resolve => setTimeout(resolve, 1200)))
    expect(requests).toBe(0)

    act(() => focusManager.setFocused(true))
    await waitFor(() => expect(requests).toBe(1))
  })
})
