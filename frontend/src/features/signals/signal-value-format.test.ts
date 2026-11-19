import type { RustSignal, RustSignalField } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import { formatSignalValue, isNumericField } from './signal-value-format'

function signal(signal_id: string): RustSignal {
  return {
    signal_id,
    name: signal_id,
    description: '',
    location: 'internal',
    scope: { kind: 'underlying', underlying: 'btc' },
    live: true,
    quality: { state: 'ready', reasons: [] },
    fields: [],
  }
}

function field(
  name: string,
  type: RustSignalField['type'],
  value: RustSignalField['value']
): RustSignalField {
  return { name, label: name, type, headline: true, value }
}

const price = signal('btc-5m.current.price.v1')
const quote = signal('btc.coinbase.spot_quote.v1')
const flow = signal('btc.coinbase.micro_momentum.v1')

describe('formatSignalValue by concept', () => {
  it('reads book probabilities as cents and spot quotes as dollars', () => {
    expect(formatSignalValue(price, field('bid', 'decimal', '0.435'))).toBe('43.5¢')
    expect(formatSignalValue(price, field('up', 'decimal', '0.6'))).toBe('60¢')
    expect(formatSignalValue(quote, field('bid', 'decimal', '79864.93'))).toBe('79,864.93')
    expect(
      formatSignalValue(
        signal('btc.kalshi.prediction_quote.v1'),
        field('affirmative_ask', 'decimal', '0.52')
      )
    ).toBe('52¢')
  })

  it('reads probabilities as percentages', () => {
    const fair = signal('btc.polymarket.fair_up_probability.v1')
    expect(formatSignalValue(fair, field('probability', 'decimal', '0.4351'))).toBe('43.5%')
  })

  it('signs basis points, probability points and z-scores', () => {
    expect(formatSignalValue(flow, field('return_bps_10s', 'decimal', '0.8495'))).toBe('+0.85 bps')
    expect(formatSignalValue(flow, field('return_bps_10s', 'decimal', '-1.2'))).toBe('-1.2 bps')
    expect(formatSignalValue(flow, field('return_bps_10s', 'decimal', '0'))).toBe('0 bps')
    expect(formatSignalValue(flow, field('return_pp_30s', 'decimal', '2.5'))).toBe('+2.5 pp')
    expect(formatSignalValue(flow, field('z_1m', 'decimal', '1.234'))).toBe('+1.23')
    expect(formatSignalValue(flow, field('imbalance', 'decimal', '-0.0608'))).toBe('-0.06')
  })

  it('reads ratios as change against the baseline', () => {
    expect(formatSignalValue(flow, field('depth_ratio_60s', 'decimal', '1.0047'))).toBe('+0.5%')
    expect(formatSignalValue(flow, field('velocity_ratio', 'decimal', '1.3384'))).toBe('+33.8%')
    expect(formatSignalValue(flow, field('spread_ratio_60s', 'decimal', '0.15'))).toBe('-85%')
    expect(formatSignalValue(flow, field('spread_ratio_60s', 'decimal', '1'))).toBe('0%')
  })

  it('reads durations at the scale they are at', () => {
    expect(formatSignalValue(flow, field('recovery_ms', 'duration_ms', 340))).toBe('340 ms')
    expect(formatSignalValue(flow, field('recovery_ms', 'duration_ms', 4200))).toBe('4.2 s')
    expect(formatSignalValue(flow, field('remaining', 'duration_ms', 113_839))).toBe('1:54')
  })

  it('reads states as words with arrows on directions', () => {
    expect(formatSignalValue(flow, field('state', 'enum', 'within_guard'))).toBe('Within guard')
    expect(formatSignalValue(flow, field('direction_10s', 'enum', 'up'))).toBe('↑ Up')
    expect(formatSignalValue(flow, field('aggressor_side', 'enum', 'sell'))).toBe('↓ Sell')
    expect(formatSignalValue(flow, field('near_tie', 'bool', true))).toBe('Yes')
  })

  it('keeps dollar notionals as money and large decimals to two places', () => {
    expect(formatSignalValue(flow, field('signed_notional_usd_10s', 'decimal', '-1234.5'))).toBe(
      '-$1,234.50'
    )
    expect(formatSignalValue(flow, field('microprice', 'decimal', '79867.5535'))).toBe('79,867.55')
    expect(formatSignalValue(flow, field('rsi_14', 'decimal', '61.2345678'))).toBe('61.2346')
  })

  it('knows which fields a sparkline can trace', () => {
    expect(isNumericField(field('rsi_14', 'decimal', '1'))).toBe(true)
    expect(isNumericField(field('trades', 'u32', 1))).toBe(true)
    expect(isNumericField(field('state', 'enum', 'quiet'))).toBe(false)
    expect(isNumericField(field('near_tie', 'bool', true))).toBe(false)
  })
})
