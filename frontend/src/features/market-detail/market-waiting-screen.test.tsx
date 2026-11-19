import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

const { router } = vi.hoisted(() => ({ router: { refresh: vi.fn() } }))
vi.mock('next/navigation', () => ({ useRouter: () => router }))

import { MarketWaitingScreen } from './market-waiting-screen'

const NO_GAME = 'No game is open right now. The next one opens when Gemini lists it.'

afterEach(() => {
  router.refresh.mockClear()
  vi.useRealTimers()
})

describe('market waiting screen', () => {
  it('shows the series title and the no-game sentence', async () => {
    const { container } = render(<MarketWaitingScreen title="NBA game winner" />)

    expect(screen.getByRole('heading', { name: 'NBA game winner' })).toBeVisible()
    expect(screen.getByText(NO_GAME)).toBeVisible()
    await expectNoA11yViolations(container)
  })

  it('refreshes the route every 15 seconds and stops on unmount', () => {
    vi.useFakeTimers()
    const { unmount } = render(<MarketWaitingScreen title="NBA game winner" />)

    expect(router.refresh).not.toHaveBeenCalled()
    vi.advanceTimersByTime(15_000)
    expect(router.refresh).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(15_000)
    expect(router.refresh).toHaveBeenCalledTimes(2)

    unmount()
    vi.advanceTimersByTime(30_000)
    expect(router.refresh).toHaveBeenCalledTimes(2)
  })
})
