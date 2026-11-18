import { describe, expect, it } from 'vitest'

import { fieldPresentation, shareAxis } from './signal-presentation'

const decimal = (name: string) => ({ name, type: 'decimal' as const })

describe('fieldPresentation', () => {
  it('plots book probabilities as cents on a fixed 0–100 axis', () => {
    const up = fieldPresentation('btc-2m.current.price.v1', decimal('up'))
    expect(up.unit).toBe('cents')
    expect(up.scale).toBe(100)
    expect(up.domain).toEqual([0, 100])
    expect(up.formatValue(0.6 * up.scale)).toBe('60¢')
    expect(up.axisFormat).toEqual({ style: 'number', maximumFractionDigits: 0 })
  })

  it('plots spot and strike in dollars, sharing one axis', () => {
    const spot = fieldPresentation('pyth.btc_usd.spot.v1', decimal('last'))
    const strike = fieldPresentation('btc-2m.current.strike.v1', decimal('price'))
    const ema = fieldPresentation('btc.coinbase.ema_cross.v1', decimal('ema_9'))
    expect(spot.unit).toBe('usd')
    expect(strike.unit).toBe('usd')
    expect(shareAxis(spot, strike)).toBe(true)
    expect(shareAxis(spot, ema)).toBe(true)
    expect(spot.formatValue(79_845.74)).toBe('$79,845.74')
  })

  it('plots venue mids and the blended reference in dollars on one axis', () => {
    const coinbase = fieldPresentation('btc.external.spot_reference.v1', decimal('coinbase_mid'))
    const kraken = fieldPresentation('btc.external.spot_reference.v1', decimal('kraken_mid'))
    const reference = fieldPresentation(
      'btc.external.spot_reference.v1',
      decimal('reference_price')
    )
    const quote = fieldPresentation('btc.kraken.spot_quote.v1', decimal('midpoint'))
    expect([coinbase, kraken, reference].every(field => field.unit === 'usd')).toBe(true)
    expect(shareAxis(coinbase, kraken)).toBe(true)
    expect(shareAxis(reference, quote)).toBe(true)
  })

  it('keeps RSI on 0–100 with oversold and overbought guides', () => {
    const rsi = fieldPresentation('btc.coinbase.rsi.v1', decimal('rsi_14'))
    expect(rsi.domain).toEqual([0, 100])
    expect(rsi.guides).toEqual([
      { value: 30, label: 'Oversold' },
      { value: 70, label: 'Overbought' },
    ])
    expect(rsi.formatValue(38.24)).toBe('38.24')
  })

  it('reads Bollinger %B as a probability-scale number with band guides', () => {
    const percentB = fieldPresentation('btc.kraken.bollinger.v1', decimal('percent_b'))
    expect(percentB.unit).toBe('probability')
    expect(percentB.scale).toBe(100)
    expect(percentB.domain).toEqual([-25, 125])
    expect(percentB.guides?.map(guide => guide.value)).toEqual([0, 100])
    expect(percentB.formatValue(0.612 * percentB.scale)).toBe('61.2%')
  })

  it('classifies signed units and gives each a zero guide', () => {
    const bpsField = fieldPresentation('btc.coinbase.roc.v1', decimal('roc_bps_1m'))
    const ppField = fieldPresentation('btc.kalshi.probability_momentum.v1', decimal('return_pp_1m'))
    const pctField = fieldPresentation('btc.bybit.open_interest_delta.v1', decimal('delta_pct_1m'))
    const zField = fieldPresentation('btc.coinbase.zscore.v1', decimal('z_1m'))
    expect(bpsField.unit).toBe('bps')
    expect(bpsField.formatValue(12.34)).toBe('+12.3 bps')
    expect(bpsField.guides).toEqual([{ value: 0 }])
    expect(ppField.unit).toBe('pp')
    expect(ppField.formatValue(-1.5)).toBe('-1.5 pp')
    expect(pctField.unit).toBe('pct')
    expect(zField.unit).toBe('plain')
    expect(zField.formatValue(1.25)).toBe('+1.25')
    expect(zField.guides).toEqual([{ value: 0 }])
    expect(shareAxis(zField, zField)).toBe(false)
  })

  it('bounds imbalance to −1..1 and reads ratios against one', () => {
    const imbalance = fieldPresentation(
      'btc.coinbase.actionable_depth_imbalance.v1',
      decimal('imbalance')
    )
    const ratio = fieldPresentation('btc.coinbase.liquidity_shock.v1', decimal('depth_ratio_60s'))
    expect(imbalance.domain).toEqual([-1, 1])
    expect(ratio.unit).toBe('ratio')
    expect(ratio.guides).toEqual([{ value: 1 }])
    expect(ratio.formatValue(1.5)).toBe('1.5×')
  })

  it('handles the round stats, counts, durations and dollar notionals', () => {
    const volume = fieldPresentation('btc-2m.current.stats.v1', decimal('volume'))
    const lots = fieldPresentation('btc-2m.current.stats.v1', {
      name: 'open_interest',
      type: 'u64',
    })
    const remaining = fieldPresentation('btc-2m.current.window.v1', {
      name: 'remaining',
      type: 'duration_ms',
    })
    const notional = fieldPresentation(
      'btc.external.aggressor_flow.v1',
      decimal('signed_notional_usd_10s')
    )
    expect(volume.unit).toBe('usd')
    expect(volume.formatValue(2365)).toBe('$2,365.00')
    expect(lots.unit).toBe('count')
    expect(lots.formatValue(1512)).toBe('1,512')
    expect(remaining.unit).toBe('ms')
    expect(notional.unit).toBe('usd')
  })

  it('falls back to a plain number for states and unknown fields', () => {
    expect(fieldPresentation('btc.coinbase.rsi.v1', { name: 'regime', type: 'enum' }).unit).toBe(
      'plain'
    )
    expect(fieldPresentation('x.y.z.v1', decimal('mystery')).formatValue(123.456)).toBe('123.46')
    expect(fieldPresentation('x.y.z.v1', decimal('mystery')).formatValue(0.123456)).toBe('0.1235')
  })
})
