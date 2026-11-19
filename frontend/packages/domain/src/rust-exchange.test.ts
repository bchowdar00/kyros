import { describe, expect, it } from 'vitest'

import {
  RustAccountsFrameSchema,
  RustBookFrameSchema,
  RustPlaceOutcomeSchema,
  RustPositionFrameSchema,
  RustPriceFrameSchema,
  RustRecordViewSchema,
  RustSeriesListSchema,
  RustSignalsHistorySchema,
  RustSignalsMessageSchema,
  RustStrategyTemplateCatalogueSchema,
  RustTradesFrameSchema,
  RustUserTradesSchema,
  tenthsToMicros,
} from './rust-exchange'

const template = {
  id: 'fair-value-discount',
  title: 'Buy the side the model says is cheap',
  series: ['btc-5m', 'btc-15m'],
  creator: { handle: '@kyros', username: 'Kyros', avatar: 'strategy-01.jpg' },
  envelope: { loss_budget_fraction: '0.5', max_order_size: '50', max_open_orders: '2' },
  primary: {
    name: 'Fair Value Discount',
    logic: 'Buy Up under fair.',
    running: 3,
    deployed_tenths: 3_000_000,
    ready: true,
    waiting_on: [],
  },
  inverse: {
    name: 'Fair Value Discount (Down)',
    logic: 'Buy Down under fair.',
    running: 1,
    deployed_tenths: 500_000,
    ready: false,
    waiting_on: [{ signal_id: 'btc-5m.current.delta.v1', name: 'Strike Delta' }],
  },
  running: 4,
  deployed_tenths: 3_500_000,
  runners: ['participant-01.png'],
  ready: false,
  waiting_on: [{ signal_id: 'btc-5m.current.delta.v1', name: 'Strike Delta' }],
}

const address = `0x${'ab'.repeat(20)}`

/** An accounts-frame row; fields the UI does not read are still allowed on the wire. */
const accountRow = (id: string, kind: 'main' | 'strategy') => ({
  id,
  kind,
  name: kind === 'main' ? 'jet' : 'Momentum',
  status: kind === 'main' ? null : 'active',
  home_series: kind === 'main' ? null : 'btc-5m',
  trades_this_market: 1,
  series_positions: [],
  balance_tenths: 700_000,
  locked_tenths: 620,
  market_pnl_total_tenths: 150,
  lifetime_pnl_tenths: 150,
  equity_tenths: 700_620,
})

describe('rust exchange wire', () => {
  it('parses the Gemini block on a series: environment, contract, phase and book freshness', () => {
    const parsed = RustSeriesListSchema.safeParse([
      {
        series: 'btc-5m',
        duration_secs: 300,
        underlying: 'btc',
        outcomes: ['up', 'down'],
        current_market_id: 8,
        opens_at_ms: 1_795_035_300_000,
        closes_at_ms: 1_795_035_600_000,
        reference: { strike_btc_usd: 62_525.42 },
        gemini: {
          env: 'production',
          series: 'BTC05M',
          phase: 'open',
          book: 'live',
          event_ticker: 'BTC05M2611182100',
          symbol: 'GEMI-BTC05M2611182100-UP',
          strike: '62525.42',
          starts_at_ms: 1_795_035_300_000,
          expires_at_ms: 1_795_035_600_000,
        },
      },
      {
        series: 'btc-15m',
        duration_secs: 900,
        current_market_id: null,
        opens_at_ms: null,
        closes_at_ms: null,
        gemini: { env: 'sandbox', series: 'BTC15M', phase: 'waiting_for_window', book: 'waiting' },
      },
    ])
    expect(parsed.success).toBe(true)
    expect(parsed.success && parsed.data[0]?.gemini?.symbol).toBe('GEMI-BTC05M2611182100-UP')
    expect(parsed.success && parsed.data[1]?.gemini?.env).toBe('sandbox')
  })

  it('refuses a Gemini block whose environment, phase or book state it does not know', () => {
    const series = {
      series: 'btc-5m',
      duration_secs: 300,
      current_market_id: null,
      opens_at_ms: null,
      closes_at_ms: null,
    }
    const gemini = { env: 'production', series: 'BTC05M', phase: 'open', book: 'live' }
    for (const wrong of [{ env: 'testnet' }, { phase: 'settling' }, { book: 'fresh' }]) {
      expect(
        RustSeriesListSchema.safeParse([{ ...series, gemini: { ...gemini, ...wrong } }]).success
      ).toBe(false)
    }
  })

  it('parses a deployment record before and after its rounds are sealed on chain', () => {
    const record = {
      deployment_id: `0x${'a1'.repeat(32)}`,
      account: `0x${'74'.repeat(20)}`,
      series: 'btc-5m',
      deployment: 'Strategy_2b60ed4a0cfd:3',
      strategy_id: 'Strategy_2b60ed4a0cfd',
      program_hash: `0x${'2b'.repeat(32)}`,
      budget_cents: 4000,
      open_tx: null,
      seals: [],
    }
    expect(RustRecordViewSchema.safeParse(record).success).toBe(true)
    const sealed = RustRecordViewSchema.safeParse({
      ...record,
      open_tx: `0x${'0f'.repeat(32)}`,
      seals: [
        {
          round_id: 8,
          won: false,
          pnl_cents: -125,
          journal_hash: `0x${'c3'.repeat(32)}`,
          tx_hash: `0x${'d4'.repeat(32)}`,
          lines: 14,
        },
        {
          round_id: 9,
          won: true,
          pnl_cents: 310,
          journal_hash: `0x${'e5'.repeat(32)}`,
          tx_hash: null,
          lines: 6,
        },
      ],
    })
    expect(sealed.success && sealed.data.seals[0]?.pnl_cents).toBe(-125)
    expect(sealed.success && sealed.data.seals[1]?.tx_hash).toBeNull()
    expect(
      RustRecordViewSchema.safeParse({ ...record, open_tx: 'not-a-transaction-hash' }).success
    ).toBe(false)
  })

  it('parses the series list including a series that has not opened', () => {
    const parsed = RustSeriesListSchema.safeParse([
      {
        series: 'btc-5m',
        duration_secs: 300,
        current_market_id: 3,
        opens_at_ms: 1_788_600_000_000,
        closes_at_ms: 1_788_600_300_000,
      },
      {
        series: 'btc-15m',
        duration_secs: 900,
        current_market_id: null,
        opens_at_ms: null,
        closes_at_ms: null,
      },
    ])
    expect(parsed.success).toBe(true)
  })

  it('parses every /ws/price frame kind', () => {
    for (const frame of [
      {
        type: 'snapshot',
        series: 'btc-5m',
        duration_secs: 300,
        market_id: 3,
        strike: 79_650.5,
        opens_at_ms: 1,
        closes_at_ms: 300_001,
        ticks: [{ ts_ms: 2, price: 79_651 }],
      },
      { type: 'tick', ts_ms: 3, price: 79_652.25 },
      {
        type: 'market_reset',
        series: 'btc-5m',
        market_id: 4,
        strike: 79_700,
        opens_at_ms: 300_001,
        closes_at_ms: 600_001,
        duration_secs: 300,
      },
    ]) {
      expect(RustPriceFrameSchema.safeParse(frame).success).toBe(true)
    }
  })

  it('rejects book prices off the 1..999 range', () => {
    const book = { type: 'snapshot', series: 'btc-5m', bids: [{ price: 500, qty: 10 }], asks: [] }
    expect(RustBookFrameSchema.safeParse(book).success).toBe(true)
    expect(
      RustBookFrameSchema.safeParse({ ...book, bids: [{ price: 1000, qty: 1 }] }).success
    ).toBe(false)
  })

  it('parses the round, midpoint and join history on a book frame', () => {
    const book = { type: 'snapshot', series: 'btc-5m', bids: [], asks: [] }
    const joined = RustBookFrameSchema.safeParse({
      ...book,
      market_id: 7,
      mid: 450,
      mid_history: [{ ts_ms: 1_000, mid: 400 }],
    })
    expect(joined.success).toBe(true)
    expect(joined.data?.mid_history).toEqual([{ ts_ms: 1_000, mid: 400 }])
    // An empty book before the first round: both null, no history on a broadcast frame.
    expect(RustBookFrameSchema.safeParse({ ...book, market_id: null, mid: null }).success).toBe(
      true
    )
    expect(RustBookFrameSchema.safeParse({ ...book, mid: 1000 }).success).toBe(false)
  })

  it('parses trades, positions and order outcomes', () => {
    const identity = { address, root_address: address, username: 'jet' }
    expect(
      RustTradesFrameSchema.safeParse({
        type: 'batch',
        trades: [
          {
            series: 'btc-5m',
            price: 510,
            qty: 5,
            ts: 9,
            side: 'yes',
            taker: identity,
            maker: identity,
          },
        ],
      }).success
    ).toBe(true)
    expect(
      RustPositionFrameSchema.safeParse({
        type: 'position_snapshot',
        as_of_unix_ms: 9,
        series: 'btc-5m',
        market_id: 3,
        address,
        side: 'no',
        size_shares: 4,
        signed_position: -4,
        avg_entry_yes_tenths: 480,
        cost_basis_tenths: -2080,
        mark_yes_tenths: 470,
        unrealized_pnl_tenths: 40,
        realized_pnl_this_market_tenths: 0,
        balance_tenths: 997_920,
        locked_tenths: 2080,
        series_positions: [],
      }).success
    ).toBe(true)
    expect(
      RustPlaceOutcomeSchema.safeParse({
        order_id: 12,
        fills: [],
        status: 'rested',
        resting_qty: 5,
      }).success
    ).toBe(true)
  })

  it('parses signal history over the socket and over REST, and the user trade log', () => {
    const frame = {
      signal_id: 'btc.coinbase.rsi.v1',
      ts_ms: 5,
      quality: { state: 'ready', reasons: [] },
      values: { rsi_14: '38.2', regime: 'neutral' },
    }
    const history = [{ signal_id: 'btc.coinbase.rsi.v1', frames: [frame] }]
    expect(RustSignalsMessageSchema.safeParse({ kind: 'history', signals: history }).success).toBe(
      true
    )
    expect(RustSignalsMessageSchema.safeParse({ kind: 'unknown', frames: [] }).success).toBe(false)
    expect(
      RustSignalsHistorySchema.safeParse({ series: 'btc-5m', since_ms: 0, signals: history })
        .success
    ).toBe(true)
    expect(
      RustUserTradesSchema.safeParse({
        trades: [
          {
            series: 'btc-5m',
            order_id: 9,
            ts: 10,
            side: 'yes',
            qty_requested: 5,
            filled_qty: 5,
            avg_price: 520,
            source: 'ui',
            order_type: 'market',
            status: 'filled',
            market_id: 3,
            realized_pnl_tenths: 0,
            settled: false,
          },
        ],
      }).success
    ).toBe(true)
  })

  it('parses the accounts frame with its open orders and merged trade log', () => {
    const child = `0x${'cd'.repeat(20)}`
    const parsed = RustAccountsFrameSchema.safeParse({
      type: 'accounts_snapshot',
      as_of_unix_ms: 9,
      address,
      main: accountRow(address, 'main'),
      strategies: [accountRow(child, 'strategy')],
      totals: { equity_tenths: 1_401_240 },
      open_orders: [
        {
          series: 'btc-5m',
          order_id: 4,
          market_id: 3,
          account: child,
          account_kind: 'strategy',
          account_name: 'Momentum',
          side: 'bid',
          price: 310,
          qty_open: 2,
          placed_ts: 12,
        },
      ],
      trades: [
        {
          series: 'btc-5m',
          order_id: 3,
          ts: 11,
          side: 'no',
          qty_requested: 3,
          filled_qty: 3,
          avg_price: 500,
          source: 'strategy',
          order_type: 'market',
          status: 'filled',
          market_id: 3,
          strategy_id: child,
          strategy_name: 'Momentum',
          realized_pnl_tenths: 0,
          settled: false,
        },
      ],
    })
    expect(parsed.success).toBe(true)
    expect(parsed.data?.open_orders[0]?.account).toBe(child)
    expect(parsed.data?.trades[0]?.strategy_name).toBe('Momentum')
  })

  it('parses the strategy template catalogue, with and without a portrait', () => {
    const parsed = RustStrategyTemplateCatalogueSchema.safeParse({
      series: 'btc-5m',
      templates: [
        template,
        { ...template, id: 'absorption-fade', creator: { ...template.creator, avatar: null } },
      ],
    })
    expect(parsed.success).toBe(true)
    expect(parsed.success && parsed.data.templates[0]?.waiting_on?.[0]?.name).toBe('Strike Delta')
    expect(
      RustStrategyTemplateCatalogueSchema.safeParse({
        series: 'btc-5m',
        templates: [{ ...template, primary: undefined }],
      }).success
    ).toBe(false)
  })

  it('parses a template from an exchange that predates readiness', () => {
    const { ready, waiting_on, ...legacy } = template
    expect(ready).toBe(false)
    expect(waiting_on).toHaveLength(1)
    const parsed = RustStrategyTemplateCatalogueSchema.safeParse({
      series: 'btc-5m',
      templates: [legacy],
    })
    expect(parsed.success && parsed.data.templates[0]?.ready).toBeUndefined()
    expect(
      RustStrategyTemplateCatalogueSchema.safeParse({
        series: 'btc-5m',
        templates: [{ ...template, waiting_on: [{ signal_id: 'x' }] }],
      }).success
    ).toBe(false)
  })

  it('converts tenths of a cent to exact micro-dollar strings', () => {
    expect(tenthsToMicros(1_000_000)).toBe('1000000000')
    expect(tenthsToMicros(1)).toBe('1000')
  })
})
