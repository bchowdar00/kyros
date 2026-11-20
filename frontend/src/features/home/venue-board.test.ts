import { describe, expect, it } from 'vitest'

import fixture from './venue-board.fixture.json'
import { PER_LEAGUE, toVenueGames, type VenueGame, withoutListed } from './venue-board'

// The fixture is a trimmed real Kalshi `/events` body read on 2026-11-20: two NFL games, one
// Premier League game (its Fulham market edited to `closed`), and a one-market event.
const bodyOf = (...tickers: string[]) => ({
  events: fixture.events.filter(event => tickers.includes(event.event_ticker)),
})

const byId = (games: VenueGame[], id: string): VenueGame | undefined =>
  games.find(game => game.id === id)

// A two-market NFL-shaped body whose second market can be overridden to break one rule at a time.
const bad = (over: object) => ({
  events: [
    {
      event_ticker: 'T',
      title: 'A vs B',
      markets: [
        {
          ticker: 'm-a',
          status: 'active',
          market_type: 'binary',
          yes_sub_title: 'A',
          yes_bid_dollars: '0.40',
          yes_ask_dollars: '0.50',
          volume_24h_fp: '1',
        },
        {
          ticker: 'm-b',
          status: 'active',
          market_type: 'binary',
          yes_sub_title: 'B',
          yes_bid_dollars: '0.45',
          yes_ask_dollars: '0.55',
          volume_24h_fp: '1',
          ...over,
        },
      ],
    },
  ],
})

// A two-market event at a given id and 24-hour volume, for the sort and cap test.
const tie = (ticker: string, volume: string) => ({
  event_ticker: ticker,
  title: 'A vs B',
  markets: [
    {
      ticker: `${ticker}-a`,
      status: 'active',
      market_type: 'binary',
      yes_sub_title: 'A',
      yes_bid_dollars: '0.40',
      yes_ask_dollars: '0.50',
      volume_24h_fp: volume,
    },
    {
      ticker: `${ticker}-b`,
      status: 'active',
      market_type: 'binary',
      yes_sub_title: 'B',
      yes_bid_dollars: '0.50',
      yes_ask_dollars: '0.60',
      volume_24h_fp: '0',
    },
  ],
})

describe('toVenueGames', () => {
  it('reads a game, its outcomes and prices from a real Kalshi body', () => {
    const [game] = toVenueGames(bodyOf('KXNFLGAME-26NOV22ARIKC'))

    expect(game).toEqual({
      id: 'KXNFLGAME-26NOV22ARIKC',
      title: 'Arizona vs Kansas City',
      when: 'Nov 22',
      outcomes: [
        { label: 'Arizona', percent: 8.5 },
        { label: 'Kansas City', percent: 91.5 },
      ],
      volume24h: expect.closeTo(55481.84, 2),
    })
  })

  it('gives [] for a body that is not the expected shape, and never throws', () => {
    expect(toVenueGames(null)).toEqual([])
    expect(toVenueGames({ events: 'nope' })).toEqual([])
    expect(toVenueGames({})).toEqual([])
    expect(toVenueGames(42)).toEqual([])
  })

  it('skips an event it cannot read without losing the others', () => {
    const games = toVenueGames({
      events: [{ junk: true }, ...bodyOf('KXNFLGAME-26NOV22ARIKC').events],
    })

    expect(games.map(game => game.id)).toEqual(['KXNFLGAME-26NOV22ARIKC'])
  })

  it('leaves out a market that is not active, binary and priced inside (0,1)', () => {
    // The Premier League game keeps Manchester City and Tie; Fulham's market is `closed`.
    const [game] = toVenueGames(bodyOf('KXEPLGAME-26NOV21MCIFUL'))

    expect(game?.outcomes.map(outcome => outcome.label)).not.toContain('Fulham')
    expect(game?.volume24h).toBeCloseTo(61094.27, 2)

    // A non-binary market, a bid >= ask, a bid of 0 and an ask of 1 are all left out, so the
    // game drops below two outcomes and is gone.
    expect(toVenueGames(bad({ market_type: 'scalar' }))).toEqual([])
    expect(toVenueGames(bad({ yes_bid_dollars: '0.60', yes_ask_dollars: '0.55' }))).toEqual([])
    expect(toVenueGames(bad({ yes_bid_dollars: '0' }))).toEqual([])
    expect(toVenueGames(bad({ yes_ask_dollars: '1' }))).toEqual([])
  })

  it('leaves out a game when one of its books is too wide to call a price', () => {
    // Bids near 0.10 and asks near 0.80: the middle says 45% for each side of one game.
    const wide = bad({ yes_bid_dollars: '0.09', yes_ask_dollars: '0.79' })

    expect(toVenueGames(wide)).toEqual([])
    // A ten-cent book is still a price.
    expect(toVenueGames(bad({}))).toHaveLength(1)
  })

  it('orders two fighters by the title when the title opens with the name of the card', () => {
    const fight = bad({})
    const [event] = fight.events
    if (event === undefined) throw new Error('missing event')
    event.title = 'Fight Night: B vs A'

    expect(toVenueGames(fight)[0]?.outcomes.map(outcome => outcome.label)).toEqual(['B', 'A'])
  })

  it('leaves out a game with fewer than two outcomes', () => {
    // The one-market event in the fixture never becomes a card.
    expect(toVenueGames(bodyOf('KXNFLGAME-26NOV22MIABUF'))).toEqual([])
  })

  it('orders outcomes by the title, then Tie, then anything else in the order given', () => {
    const [epl] = toVenueGames(bodyOf('KXEPLGAME-26NOV21MCIFUL'))
    expect(epl?.outcomes.map(outcome => outcome.label)).toEqual(['Manchester City', 'Tie'])

    const [game] = toVenueGames({
      events: [
        {
          event_ticker: 'ORD',
          title: 'Home vs Away',
          markets: [
            {
              ticker: '1',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Extra time',
              yes_bid_dollars: '0.10',
              yes_ask_dollars: '0.11',
              volume_24h_fp: '1',
            },
            {
              ticker: '2',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Tie',
              yes_bid_dollars: '0.20',
              yes_ask_dollars: '0.21',
              volume_24h_fp: '1',
            },
            {
              ticker: '3',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Away',
              yes_bid_dollars: '0.30',
              yes_ask_dollars: '0.31',
              volume_24h_fp: '1',
            },
            {
              ticker: '4',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Home',
              yes_bid_dollars: '0.40',
              yes_ask_dollars: '0.41',
              volume_24h_fp: '1',
            },
            {
              ticker: '5',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Abandoned',
              yes_bid_dollars: '0.01',
              yes_ask_dollars: '0.02',
              volume_24h_fp: '1',
            },
          ],
        },
      ],
    })
    expect(game?.outcomes.map(outcome => outcome.label)).toEqual([
      'Home',
      'Away',
      'Tie',
      'Extra time',
      'Abandoned',
    ])
  })

  it('reads `when` from the trailing parentheses, undefined when there are none', () => {
    expect(
      byId(toVenueGames(bodyOf('KXEPLGAME-26NOV21MCIFUL')), 'KXEPLGAME-26NOV21MCIFUL')?.when
    ).toBe('Nov 21')

    const [game] = toVenueGames({
      events: [
        {
          event_ticker: 'NOWHEN',
          title: 'A vs B',
          sub_title: 'A against B, no date',
          markets: [
            {
              ticker: '1',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'A',
              yes_bid_dollars: '0.40',
              yes_ask_dollars: '0.50',
              volume_24h_fp: '1',
            },
            {
              ticker: '2',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'B',
              yes_bid_dollars: '0.50',
              yes_ask_dollars: '0.60',
              volume_24h_fp: '1',
            },
          ],
        },
      ],
    })
    expect(game?.when).toBeUndefined()
  })

  it('sums the outcomes volume, counting a missing or unparseable figure as 0', () => {
    const [game] = toVenueGames({
      events: [
        {
          event_ticker: 'VOL',
          title: 'A vs B',
          markets: [
            {
              ticker: '1',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'A',
              yes_bid_dollars: '0.40',
              yes_ask_dollars: '0.50',
              volume_24h_fp: '12.50',
            },
            {
              ticker: '2',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'B',
              yes_bid_dollars: '0.50',
              yes_ask_dollars: '0.60',
              volume_24h_fp: 'oops',
            },
            {
              ticker: '3',
              status: 'active',
              market_type: 'binary',
              yes_sub_title: 'Tie',
              yes_bid_dollars: '0.05',
              yes_ask_dollars: '0.06',
            },
          ],
        },
      ],
    })
    expect(game?.volume24h).toBeCloseTo(12.5, 2)
  })

  it('sorts games by 24-hour volume descending, then by id, capped at PER_LEAGUE', () => {
    const board = toVenueGames(fixture)

    // The Premier League game has the most volume, then Arizona/Kansas City, then
    // Baltimore/Carolina; the one-market Miami/Buffalo event is dropped.
    expect(board.map(game => game.id)).toEqual([
      'KXEPLGAME-26NOV21MCIFUL',
      'KXNFLGAME-26NOV22ARIKC',
      'KXNFLGAME-26NOV22BALCAR',
    ])

    const many = Array.from({ length: 9 }, (_, index) => tie(`G${8 - index}`, '5'))
    const capped = toVenueGames({ events: many })
    expect(capped).toHaveLength(PER_LEAGUE)
    // Equal volume, so the ids break the tie in ascending order.
    expect(capped.map(game => game.id)).toEqual(['G0', 'G1', 'G2', 'G3', 'G4'])
  })
})

describe('withoutListed', () => {
  const game = { id: 'G', outcomes: [], title: 'A vs B', volume24h: 0, when: undefined }
  const board = {
    leagues: [
      { games: [game], label: 'Serie A' },
      { games: [game], label: 'UFC' },
    ],
  }

  it('drops a league Gemini lists itself, whatever the case of its name', () => {
    expect(withoutListed(board, ['NBA', 'serie a'])?.leagues.map(league => league.label)).toEqual([
      'UFC',
    ])
  })

  it('drops a league Gemini lists under another name for it', () => {
    const all = {
      leagues: ['Bundesliga', 'Serie A', 'Ligue 1', 'MLS', 'UFC'].map(label => ({
        games: [game],
        label,
      })),
    }
    const listed = ['Italy Serie A', 'MMA', 'Major League Soccer', 'German Bundesliga']

    expect(withoutListed(all, listed)?.leagues.map(league => league.label)).toEqual(['Ligue 1'])
  })

  it('does not take a name that only contains a league name for that league', () => {
    expect(withoutListed(board, ['Commodities', 'Summary', 'Reserie Awards'])).toEqual(board)
  })

  it('keeps every league when Gemini lists none of them', () => {
    expect(withoutListed(board, ['NBA', 'Politics'])).toEqual(board)
  })

  it('is nothing when Gemini lists them all', () => {
    expect(withoutListed(board, ['Serie A', 'UFC'])).toBeUndefined()
  })
})
