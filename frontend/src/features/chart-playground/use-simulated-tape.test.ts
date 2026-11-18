import { describe, expect, it } from 'vitest'
import { appendDueTicks } from './use-simulated-tape'

describe('appendDueTicks', () => {
  const last = { time: 1_000, value: 50 }
  const options = {
    intervalMs: 250,
    last,
    maxTicks: 3,
    random: () => 0.5,
    step: (point: { value: number }) => point.value + 1,
  }

  it('adds one tick per whole interval that has passed', () => {
    expect(appendDueTicks([], { ...options, now: 1_200 })).toEqual([])
    expect(appendDueTicks([], { ...options, now: 1_250 })).toEqual([{ time: 1_250, value: 51 }])
    expect(appendDueTicks([], { ...options, now: 1_800 })).toEqual([
      { time: 1_250, value: 51 },
      { time: 1_500, value: 52 },
      { time: 1_750, value: 53 },
    ])
  })

  it('keeps the tape from growing without bound', () => {
    const ticks = appendDueTicks([], { ...options, now: 2_500 })

    expect(ticks).toHaveLength(3)
    expect(ticks.at(-1)).toEqual({ time: 2_500, value: 56 })
  })
})
