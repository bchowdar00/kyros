import { describe, expect, it } from 'vitest'

import {
  BOOK_DEPTH_MAX,
  BOUND_KINDS,
  CHART_REFS_MAX,
  MARKDOWN_MAX,
  parseWidgetSpec,
  refKey,
  TABLE_ROWS_MAX,
  WINDOW_MS_MAX,
} from './index'

const rsi = { type: 'signal', id: 'btc.coinbase.rsi.v1', field: 'rsi_14' } as const

describe('widget specs', () => {
  it('accepts each bound kind with its defaults left to the renderer', () => {
    const accepted = [
      { kind: 'chart', series: 'btc-5m', refs: [rsi] },
      { kind: 'chart', series: 'btc-5m', refs: [{ type: 'spot' }], show_strike: true },
      { kind: 'candles', series: 'btc-15m', bucket_ms: 30_000 },
      { kind: 'book', series: 'btc-5m', depth: 15 },
      { kind: 'market', series: 'btc-5m' },
      { kind: 'stats', series: 'btc-5m', title: 'This round' },
      { kind: 'position', series: 'btc-5m' },
      { kind: 'rounds', series: 'btc-5m', count: 20 },
      { kind: 'distribution', series: 'btc-5m' },
      { kind: 'fills', series: 'btc-5m', scope: 'mine' },
      { kind: 'ticket', series: 'btc-5m', side: 'YES', amount_cents: 2_500 },
      { kind: 'scenario', series: 'btc-5m', variable: 'btc_move', side: 'NO' },
    ]
    for (const spec of accepted) {
      const parsed = parseWidgetSpec(spec)
      expect(parsed, JSON.stringify(spec)).toBeDefined()
      if (parsed) expect(BOUND_KINDS.has(parsed.kind)).toBe(true)
    }
  })

  it('accepts content and interactive kinds with inline data', () => {
    for (const spec of [
      { kind: 'markdown', text: '## Read\n\nRSI is **oversold**.' },
      { kind: 'code', language: 'json', code: '{"a":1}' },
      { kind: 'sources', sources: [{ title: 'Pyth', url: 'https://docs.pyth.network/lazer' }] },
      { kind: 'diff', from: 'v1', to: 'v2', lines: [{ kind: 'added', text: 'Cancel on RSI' }] },
      {
        kind: 'table',
        columns: [{ id: 'name', label: 'Name' }],
        rows: [['a'], [2]],
      },
      { kind: 'stat-grid', stats: [{ label: 'Edge', value: '+3¢', direction: 'up' }] },
      {
        kind: 'picker',
        prompt: 'Which side?',
        options: [
          { id: 'yes', title: 'YES' },
          { id: 'no', title: 'NO' },
        ],
      },
    ]) {
      const parsed = parseWidgetSpec(spec)
      expect(parsed, JSON.stringify(spec)).toBeDefined()
      if (parsed) expect(BOUND_KINDS.has(parsed.kind)).toBe(false)
    }
  })

  it('refuses what the renderer could not draw safely', () => {
    for (const spec of [
      { kind: 'chart', series: 'btc-5m', refs: [] },
      {
        kind: 'chart',
        series: 'btc-5m',
        refs: Array.from({ length: CHART_REFS_MAX + 1 }, () => rsi),
      },
      { kind: 'chart', series: 'btc-5m', refs: [rsi], window_ms: WINDOW_MS_MAX + 1 },
      { kind: 'chart', series: 'BTC 5m', refs: [rsi] },
      { kind: 'chart', series: 'btc-5m', refs: [{ type: 'market', measure: 'mid' }] },
      { kind: 'book', series: 'btc-5m', depth: BOOK_DEPTH_MAX + 1 },
      { kind: 'fills', series: 'btc-5m' },
      { kind: 'markdown', text: 'x'.repeat(MARKDOWN_MAX + 1) },
      {
        kind: 'table',
        columns: [{ id: 'a', label: 'A' }],
        rows: Array.from({ length: TABLE_ROWS_MAX + 1 }, () => ['x']),
      },
      { kind: 'picker', prompt: 'One?', options: [{ id: 'only', title: 'Only' }] },
      { kind: 'ticket', series: 'btc-5m', side: 'MAYBE' },
      { kind: 'gauge', series: 'btc-5m' },
      'not an object',
      null,
    ]) {
      expect(parseWidgetSpec(spec), JSON.stringify(spec)).toBeUndefined()
    }
  })

  it('keys references stably', () => {
    expect(refKey(rsi)).toBe('btc.coinbase.rsi.v1:rsi_14')
    expect(refKey({ type: 'market', measure: 'bid' })).toBe('market:bid')
    expect(refKey({ type: 'spot' })).toBe('spot')
    expect(refKey({ type: 'strike' })).toBe('strike')
  })
})
