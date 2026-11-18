import { describe, expect, it } from 'vitest'

import { parsePriceTick, tickToChartPoint } from './price-tick'

const symbol = 'Crypto.BTC/USD'

describe('parsePriceTick', () => {
  it('accepts a v1 tick for the subscribed symbol', () => {
    expect(
      parsePriceTick(
        {
          expo: -8,
          mantissa: '11812000000000',
          provider: 'PYTH_LAZER',
          publishTimeUs: 1_788_436_800_000_000,
          symbol,
          v: 1,
        },
        symbol
      )
    ).toEqual({
      expo: -8,
      mantissa: '11812000000000',
      publishTimeUs: 1_788_436_800_000_000,
      symbol,
      v: 1,
    })
  })

  it('accepts publishTimeUs as a decimal string', () => {
    const tick = parsePriceTick(
      { expo: 0, mantissa: '1', publishTimeUs: '1788436800000000', symbol, v: 1 },
      symbol
    )

    expect(tick?.publishTimeUs).toBe(1_788_436_800_000_000)
  })

  it('rejects a tick for another symbol', () => {
    expect(
      parsePriceTick(
        { expo: 0, mantissa: '1', publishTimeUs: 1, symbol: 'Crypto.ETH/USD', v: 1 },
        symbol
      )
    ).toBeUndefined()
  })
})

describe('tickToChartPoint', () => {
  it('maps mantissa and exponent onto a chart sample', () => {
    expect(
      tickToChartPoint({
        expo: -8,
        mantissa: '11812000000000',
        publishTimeUs: 1_788_436_800_123_000,
        symbol,
        v: 1,
      })
    ).toEqual({ time: 1_788_436_800_123, value: 118_120 })
  })
})
