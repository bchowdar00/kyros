import { act, render, screen } from '@testing-library/react'
import { afterEach, expect, it, vi } from 'vitest'

import { MarketCountdown } from './market-countdown'

afterEach(() => vi.useRealTimers())

it('counts down independently of price ticks and follows the next market deadline', () => {
  vi.useFakeTimers()
  vi.setSystemTime(new Date('2026-09-05T14:00:00Z'))
  const endsAt = Date.now() + 60_000
  const { rerender } = render(<MarketCountdown endsAt={endsAt} />)
  expect(screen.getByText('01:00')).toBeInTheDocument()

  act(() => vi.advanceTimersByTime(1000))
  expect(screen.getByText('00:59')).toBeInTheDocument()

  rerender(<MarketCountdown endsAt={endsAt + 60_000} />)
  expect(screen.getByText('01:59')).toBeInTheDocument()

  act(() => vi.advanceTimersByTime(120_000))
  expect(screen.getByText('00:00')).toBeInTheDocument()
})
