import type { RustSignal, RustSignalCatalogue } from '@kyros/domain'
import { describe, expect, it } from 'vitest'

import {
  groupCardsByVenue,
  pinnedSignalCards,
  signalDisplayName,
  signalFieldCards,
  stripSignalCards,
  venueOf,
  withTraces,
} from './signal-cards'

function signal(overrides: Partial<RustSignal> & Pick<RustSignal, 'signal_id'>): RustSignal {
  return {
    name: overrides.signal_id,
    description: '',
    location: 'internal',
    scope: { kind: 'underlying', underlying: 'btc' },
    live: true,
    quality: { state: 'ready', reasons: [] },
    fields: [],
    ...overrides,
  }
}

const stats = signal({
  signal_id: 'btc-5m.current.stats.v1',
  name: 'Round activity',
  description:
    'How much has traded in this round so far. Everything here resets when the next round opens.',
  scope: { kind: 'series', series_id: 'btc-5m' },
  fields: [
    {
      name: 'volume',
      label: 'Volume',
      type: 'decimal',
      headline: true,
      description:
        'The dollar value of every fill this round. It only goes up until the round settles.',
      value: '88650.000',
    },
    { name: 'largest_trade', label: 'Largest trade', type: 'u32', headline: true },
  ],
})

const rsi = signal({
  signal_id: 'btc.coinbase.rsi.v1',
  name: 'RSI',
  description:
    'Relative strength over the last fourteen one-minute bars, from zero to one hundred.',
  fields: [
    { name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true, value: '61.2345678' },
    { name: 'bar_count', label: 'Bar Count', type: 'u16', headline: false, value: 14 },
  ],
})

const warming = signal({
  signal_id: 'btc.kraken.rsi.v1',
  name: 'RSI',
  quality: { state: 'warming_up', reasons: [] },
  fields: [{ name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true, value: '50' }],
})

const spot = signal({
  signal_id: 'pyth.btc_usd.spot.v1',
  name: 'BTC price',
  location: 'external',
  fields: [
    { name: 'last', label: 'BTC price', type: 'decimal', headline: true, value: '100000.5' },
  ],
})

const catalogue: RustSignalCatalogue = {
  series: 'btc-5m',
  underlying: 'btc',
  signals: [rsi, warming, stats, spot],
}

describe('venueOf', () => {
  it('names the market, the lane venue, the oracle and the blend', () => {
    expect(venueOf(stats)).toBe('This market')
    expect(venueOf(rsi)).toBe('Coinbase')
    expect(venueOf(spot)).toBe('Oracles')
    expect(venueOf(signal({ signal_id: 'btc.external.macd.v1' }))).toBe('Blended')
    expect(venueOf(signal({ signal_id: 'btc.multi.pair_cost_spread.v1' }))).toBe('Cross-venue')
    expect(venueOf(signal({ signal_id: 'btc.okx.rsi.v1' }))).toBe('Okx')
  })
})

describe('signalFieldCards', () => {
  it('makes one card per valued headline field of a ready signal, in registry order', () => {
    const cards = signalFieldCards(catalogue)
    expect(cards.map(card => card.pin)).toEqual([
      'btc.coinbase.rsi.v1:rsi_14',
      'btc-5m.current.stats.v1:volume',
      'pyth.btc_usd.spot.v1:last',
    ])
    expect(cards[0]).toMatchObject({
      label: 'RSI',
      value: '61.2346',
      signalName: 'RSI',
      summary: rsi.description,
      venue: 'Coinbase',
    })
    expect(cards[0]?.description).toBeUndefined()
    expect(cards[1]).toMatchObject({
      value: '$88,650.00',
      venue: 'This market',
      description:
        'The dollar value of every fill this round. It only goes up until the round settles.',
    })
  })

  it('includes the plumbing fields when asked to show all', () => {
    const pins = signalFieldCards(catalogue, { showAll: true }).map(card => card.pin)
    expect(pins).toContain('btc.coinbase.rsi.v1:bar_count')
    expect(pins).not.toContain('btc-5m.current.stats.v1:largest_trade')
    expect(pins).not.toContain('btc.kraken.rsi.v1:rsi_14')
  })
})

describe('pinnedSignalCards', () => {
  it('resolves pins in pin order with the venue in the name, dropping unknown ones', () => {
    const quote = signal({
      signal_id: 'btc.polymarket.prediction_quote.v1',
      name: 'Venue price',
      fields: [
        {
          name: 'affirmative_bid',
          label: 'Up bid',
          type: 'decimal',
          headline: true,
          value: '0.55',
        },
      ],
    })
    const cards = pinnedSignalCards({ ...catalogue, signals: [...catalogue.signals, quote] }, [
      'btc.polymarket.prediction_quote.v1:affirmative_bid',
      'btc.coinbase.rsi.v1:rsi_14',
      'btc-5m.current.stats.v1:volume',
      'gone.v1:x',
      'btc-5m.current.stats.v1:nope',
    ])
    expect(cards).toEqual([
      {
        id: 'btc.polymarket.prediction_quote.v1:affirmative_bid',
        context: 'Polymarket venue price',
        label: 'Polymarket up bid',
        value: '55¢',
      },
      {
        id: 'btc.coinbase.rsi.v1:rsi_14',
        context: 'Coinbase RSI',
        label: 'Coinbase RSI',
        value: '61.2346',
      },
      {
        id: 'btc-5m.current.stats.v1:volume',
        context: 'Round activity',
        label: 'Volume',
        value: '$88,650.00',
      },
    ])
  })
})

describe('stripSignalCards', () => {
  it('lists the pinned readings first, then every other ready headline reading once', () => {
    const all = signalFieldCards(catalogue).map(card => card.pin)
    const pinned = all.at(-1)
    if (pinned === undefined || all.length < 2) throw new Error('the fixture needs two readings')

    const strip = stripSignalCards(catalogue, [pinned, 'gone.v1:x'])

    expect(strip.map(card => card.id)).toEqual([pinned, ...all.filter(pin => pin !== pinned)])
    expect(strip[0]).toEqual(pinnedSignalCards(catalogue, [pinned])[0])
  })

  it('is every ready headline reading in registry order when nothing is pinned', () => {
    expect(stripSignalCards(catalogue, []).map(card => card.id)).toEqual(
      signalFieldCards(catalogue).map(card => card.pin)
    )
  })
})

describe('stripSignalCards before a game has a play', () => {
  it('leaves out a reading with nothing in it, and keeps a pinned one', () => {
    const pbp = signal({
      signal_id: 'nba.live.pbp.v1',
      name: 'Live play',
      scope: { kind: 'underlying', underlying: 'nba' },
      fields: [
        { name: 'play', label: 'Play', type: 'string', headline: true, value: 'Other' },
        { name: 'team', label: 'Team', type: 'string', headline: true, value: '' },
      ],
    })
    const pregame: RustSignalCatalogue = { series: 'nba-h2h', underlying: 'nba', signals: [pbp] }

    expect(stripSignalCards(pregame, []).map(card => card.id)).toEqual(['nba.live.pbp.v1:play'])
    // What a visitor pinned stays where they put it, even while it is empty.
    expect(stripSignalCards(pregame, ['nba.live.pbp.v1:team']).map(card => card.id)).toEqual([
      'nba.live.pbp.v1:team',
      'nba.live.pbp.v1:play',
    ])
  })
})

describe('signalDisplayName', () => {
  const named: RustSignalCatalogue = {
    series: 'btc-5m',
    underlying: 'btc',
    signals: [
      signal({ signal_id: 'btc.coinbase.micro_momentum.v1', name: 'Momentum' }),
      signal({ signal_id: 'btc.external.roc.v1', name: 'Rate of change' }),
      signal({ signal_id: 'btc.polymarket.prediction_quote.v1', name: 'Venue price' }),
      stats,
    ],
  }

  it('turns a signal id into its venue and the catalogue name, and leaves other text alone', () => {
    expect(signalDisplayName('btc.coinbase.micro_momentum.v1', named)).toBe('Coinbase momentum')
    expect(signalDisplayName('btc.external.roc.v1', named)).toBe('Blended rate of change')
    expect(signalDisplayName('btc.polymarket.prediction_quote.v1', named)).toBe(
      'Polymarket venue price'
    )
    expect(signalDisplayName('btc-5m.current.stats.v1', named)).toBe('Round activity')
    expect(signalDisplayName('Coinbase', named)).toBeUndefined()
    expect(signalDisplayName('btc.coinbase.not_listed.v1', named)).toBeUndefined()
    // Without a catalogue there is no server copy to name it by.
    expect(signalDisplayName('btc.external.roc.v1', null)).toBeUndefined()
  })
})

describe('groupCardsByVenue', () => {
  it('orders venues with the market first and nests one block per signal', () => {
    const groups = groupCardsByVenue(signalFieldCards(catalogue, { showAll: true }))
    expect(groups.map(group => group.venue)).toEqual(['This market', 'Oracles', 'Coinbase'])
    expect(groups[0]).toMatchObject({ count: 1 })
    const coinbase = groups[2]
    expect(coinbase?.count).toBe(2)
    expect(coinbase?.blocks).toHaveLength(1)
    expect(coinbase?.blocks[0]).toMatchObject({
      signalId: 'btc.coinbase.rsi.v1',
      title: 'RSI',
      summary: rsi.description,
    })
    expect(coinbase?.blocks[0]?.cards.map(card => card.pin)).toEqual([
      'btc.coinbase.rsi.v1:rsi_14',
      'btc.coinbase.rsi.v1:bar_count',
    ])
  })
})

describe('withTraces', () => {
  it('gives each strip card the recent samples of its own reading, and leaves the rest bare', () => {
    const [volume, price] = stripSignalCards(catalogue, [])
    if (volume === undefined || price === undefined) throw new Error('fixture has two cards')
    const samples = [
      { time: 1, value: 1 },
      { time: 2, value: 2 },
    ]

    const traced = withTraces([volume, price], { [volume.id]: samples })

    expect(traced).toEqual([{ ...volume, trace: samples }, price])
  })
})
