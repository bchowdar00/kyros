import { describe, expect, it } from 'vitest'
import { mergePriceHistory } from './price-history.mapper'

describe('mergePriceHistory', () => {
  it('retains supplied history when the first live tick arrives and lets live corrections win', () => {
    const history = [
      { time: 1, value: 100 },
      { time: 2, value: 101 },
    ]
    expect(
      mergePriceHistory(history, [
        { time: 2, value: 102 },
        { time: 3, value: 103 },
      ])
    ).toEqual([
      { time: 1, value: 100 },
      { time: 2, value: 102 },
      { time: 3, value: 103 },
    ])
    expect(history[1]?.value).toBe(101)
    expect(mergePriceHistory(history, [])).toBe(history)
  })
  it('does not interleave refreshed sampled history into the live interval at rollover', () => {
    const ticks = [
      { time: 1_100, value: 102 },
      { time: 2_100, value: 103 },
      { time: 3_100, value: 104 },
    ]
    const refreshed = [
      { time: 1_000, value: 100 },
      { time: 2_000, value: 100 },
      { time: 3_000, value: 100 },
      { time: 4_000, value: 105 },
    ]
    expect(mergePriceHistory(refreshed, ticks)).toEqual([
      { time: 1_000, value: 100 },
      ...ticks,
      { time: 4_000, value: 105 },
    ])
  })
})
