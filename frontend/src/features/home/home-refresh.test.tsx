import { render } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { HomeRefresh } from './home-refresh'

const refresh = vi.hoisted(() => vi.fn())
vi.mock('next/navigation', () => ({ useRouter: () => ({ refresh }) }))

describe('HomeRefresh', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    refresh.mockClear()
  })
  afterEach(() => vi.useRealTimers())

  it('asks the server for the page again every ten seconds, and stops when the page is left', () => {
    const { unmount } = render(<HomeRefresh />)
    expect(refresh).not.toHaveBeenCalled()

    vi.advanceTimersByTime(10_000)
    expect(refresh).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(20_000)
    expect(refresh).toHaveBeenCalledTimes(3)

    unmount()
    vi.advanceTimersByTime(30_000)
    expect(refresh).toHaveBeenCalledTimes(3)
  })
})
