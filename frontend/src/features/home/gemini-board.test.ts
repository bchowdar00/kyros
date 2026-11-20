import { describe, expect, it } from 'vitest'

import fixture from './gemini-board.fixture.json'
import { toGameCards, toQuestionCards, toUpDownCards, withKyrosGame } from './gemini-board'

// The fixture is cut from Gemini's public list as it read on 2026-11-20.
const at = (iso: string) => Date.parse(iso)

// A readable question event; overrides supply the fields a given test varies.
const makeQuestion = (over: Record<string, unknown>) => ({
  category: 'Politics',
  contracts: [
    { label: 'Yes', marketState: 'open', prices: { buy: { yes: '0.5' } }, status: 'active' },
  ],
  status: 'active',
  type: 'binary',
  ...over,
})

describe('the up/down cards', () => {
  it('shows the round that is open now for each series, with both prices, the strike and the close', () => {
    const cards = toUpDownCards(fixture.crypto, at('2026-11-20T08:17:00Z'))

    expect(cards).toEqual([
      {
        asset: 'BTC',
        closesAt: '2026-11-20T08:20:00.000Z',
        downCents: 50,
        id: 'BTC05M',
        kind: 'updown',
        minutes: 5,
        strike: 61_806.17,
        title: 'BTC Up or Down 5m',
        tradeSlug: 'btc-5m',
        upCents: 56,
      },
      {
        asset: 'ETH',
        closesAt: '2026-11-20T08:30:00.000Z',
        downCents: 60,
        id: 'ETH15M',
        kind: 'updown',
        minutes: 15,
        strike: 1752.97,
        title: 'ETH Up or Down 15m',
        upCents: 47,
      },
    ])
  })

  it('moves to the next round once one closes, and leaves out a strike it has not been given', () => {
    const [btc] = toUpDownCards(fixture.crypto, at('2026-11-20T08:21:00Z'))

    expect(btc).toMatchObject({ closesAt: '2026-11-20T08:25:00.000Z', id: 'BTC05M' })
    expect(btc).not.toHaveProperty('strike')
    // 99 and 99 is an empty book, not a price for either side.
    expect(btc).not.toHaveProperty('upCents')
    expect(btc).not.toHaveProperty('downCents')
  })

  it('leaves out a series with no round left, and anything that is not an up/down market', () => {
    expect(toUpDownCards(fixture.crypto, at('2026-11-20T09:00:00Z'))).toEqual([])
  })

  it('skips an entry it cannot read without losing the others', () => {
    const cards = toUpDownCards([{ nonsense: true }, ...fixture.crypto], at('2026-11-20T08:17:00Z'))

    expect(cards.map(card => card.id)).toEqual(['BTC05M', 'ETH15M'])
  })
})

describe('the game cards', () => {
  it('lists upcoming and in-play games soonest first, each side with its price', () => {
    const cards = toGameCards(fixture.games, at('2026-11-20T21:00:00Z'))

    expect(cards.map(card => card.id)).toEqual([
      'LALIGA-2611202000-ESY-ATH-M',
      'NBA-2611210000-CHA-ATL-M',
      'NBA-2611210030-TOR-DET-M',
    ])
    expect(cards[0]).toEqual({
      id: 'LALIGA-2611202000-ESY-ATH-M',
      kind: 'game',
      league: 'La Liga',
      outcomes: [
        { cents: 66, label: 'Athletic' },
        { cents: 21, label: 'Tie' },
        { cents: 14, label: 'Espanyol' },
      ],
      started: true,
      startsAt: '2026-11-20T20:00:00.000Z',
      teams: ['ATH', 'ESY'],
      title: 'Espanyol vs Athletic',
    })
    expect(cards[2]).toMatchObject({ league: 'NBA', started: false, teams: ['DET', 'TOR'] })
  })

  it('drops a game that started long ago and is only waiting to be settled', () => {
    const cards = toGameCards(fixture.games, at('2026-11-20T09:00:00Z'))

    expect(cards.map(card => card.id)).not.toContain('NCAAM-2611150000-ULL-UNCW-M')
  })

  it('drops a game four hours after its start: by then it is over, whatever the sport', () => {
    // Espanyol v Athletic kicks off at 20:00Z.
    const ids = (now: string) => toGameCards(fixture.games, at(now)).map(card => card.id)

    expect(ids('2026-11-20T23:59:00Z')).toContain('LALIGA-2611202000-ESY-ATH-M')
    expect(ids('2026-11-21T00:01:00Z')).not.toContain('LALIGA-2611202000-ESY-ATH-M')
  })

  it('marks the one game Kyros trades, whichever way round its teams are given', () => {
    const cards = withKyrosGame(toGameCards(fixture.games, at('2026-11-20T09:00:00Z')), {
      slug: 'nba-h2h',
      teams: ['TOR', 'DET'],
    })

    expect(cards.filter(card => card.tradeSlug !== undefined).map(card => card.id)).toEqual([
      'NBA-2611210030-TOR-DET-M',
    ])
  })
})

describe('the question cards', () => {
  const NOW = at('2026-11-20T08:17:00Z')

  it('turns a binary event into a Yes/No card at its buy prices', () => {
    const [save] = toQuestionCards(fixture.questions, NOW).filter(card => card.id === 'SAVEACT')

    expect(save).toEqual({
      category: 'Politics',
      closesAt: '2027-01-04T23:59:00.000Z',
      id: 'SAVEACT',
      kind: 'question',
      more: 0,
      outcomes: [
        { cents: 2, label: 'Yes' },
        { cents: 99, label: 'No' },
      ],
      title: 'Will the SAVE Act become law?',
    })
  })

  it('lists a categorical event by price, keeps the top three and counts the rest', () => {
    const [ai] = toQuestionCards(fixture.questions, NOW).filter(card => card.id === 'BESTAI26')

    expect(ai).toEqual({
      category: 'Tech',
      closesAt: '2027-01-01T04:59:00.000Z',
      id: 'BESTAI26',
      kind: 'question',
      more: 4,
      outcomes: [
        { cents: 87, label: 'Gemini' },
        { cents: 11, label: 'Claude' },
        { cents: 5, label: 'ChatGPT' },
      ],
      title: 'Best AI at the end of 2026?',
    })
  })

  it('reads a 99/99 contract as an empty book, not a price for either side', () => {
    const [card] = toQuestionCards(
      [
        {
          category: 'Economics',
          contracts: [
            {
              label: 'Yes/No',
              marketState: 'open',
              prices: { buy: { no: '0.99', yes: '0.99' } },
              status: 'active',
            },
          ],
          expiryDate: '2026-12-01T00:00:00.000Z',
          status: 'active',
          ticker: 'EMPTY',
          title: 'Empty book',
          type: 'binary',
          volume24h: '0',
        },
      ],
      NOW
    )

    expect(card?.outcomes).toEqual([{ label: 'Yes' }, { label: 'No' }])
  })

  it('counts only active, open contracts as outcomes', () => {
    const [card] = toQuestionCards(
      [
        {
          category: 'Economics',
          contracts: [
            {
              label: 'Open',
              marketState: 'open',
              prices: { buy: { yes: '0.40' } },
              status: 'active',
            },
            {
              label: 'Settled',
              marketState: 'open',
              prices: { buy: { yes: '0.60' } },
              status: 'settled',
            },
            {
              label: 'Closed book',
              marketState: 'closed',
              prices: { buy: { yes: '0.50' } },
              status: 'active',
            },
          ],
          expiryDate: '2026-12-01T00:00:00.000Z',
          status: 'active',
          ticker: 'OPENONLY',
          title: 'Only the open one',
          type: 'categorical',
          volume24h: '1',
        },
      ],
      NOW
    )

    expect(card?.outcomes).toEqual([{ cents: 40, label: 'Open' }])
    expect(card?.more).toBe(0)
  })

  it('leaves out up/down rounds, sports games and anything no longer active', () => {
    const upDownRound = {
      category: 'Crypto',
      contracts: [
        {
          label: 'Up',
          marketState: 'open',
          prices: { buy: { no: '0.5', yes: '0.5' } },
          status: 'active',
        },
      ],
      expiryDate: '2026-11-20T08:20:00.000Z',
      series: 'BTC05M',
      status: 'active',
      template: 'crypto-up-down',
      ticker: 'BTC05M2611200820',
      title: 'BTC up or down',
      type: 'binary',
      volume24h: '9',
    }
    const sportsGame = {
      category: 'Sports',
      contracts: [
        { label: 'Home', marketState: 'open', prices: { buy: { yes: '0.6' } }, status: 'active' },
      ],
      expiryDate: '2026-11-22T00:00:00.000Z',
      status: 'active',
      ticker: 'NBA-GAME',
      title: 'A vs B',
      type: 'categorical',
      volume24h: '8',
    }
    const settled = {
      category: 'Politics',
      contracts: [
        { label: 'Yes', marketState: 'open', prices: { buy: { yes: '0.6' } }, status: 'active' },
      ],
      expiryDate: '2026-12-01T00:00:00.000Z',
      status: 'settled',
      ticker: 'DONE',
      title: 'Already settled',
      type: 'binary',
      volume24h: '7',
    }

    const cards = toQuestionCards([upDownRound, sportsGame, settled, ...fixture.questions], NOW)

    expect(cards.map(card => card.id)).toEqual(['BESTAI26', 'SAVEACT'])
  })

  it('skips an entry it cannot read without losing the others', () => {
    const cards = toQuestionCards([{ nonsense: true }, ...fixture.questions], NOW)

    expect(cards.map(card => card.id)).toEqual(['BESTAI26', 'SAVEACT'])
  })

  it('leaves out an event whose expiry has already passed', () => {
    const expired = {
      category: 'Politics',
      contracts: [
        { label: 'Yes', marketState: 'open', prices: { buy: { yes: '0.6' } }, status: 'active' },
      ],
      expiryDate: '2026-11-01T00:00:00.000Z',
      status: 'active',
      ticker: 'PAST',
      title: 'Last month',
      type: 'binary',
      volume24h: '99',
    }

    expect(toQuestionCards([expired], NOW).map(card => card.id)).toEqual([])
  })

  it('orders by 24-hour volume, then by close, then by title', () => {
    const cards = toQuestionCards(
      [
        makeQuestion({
          expiryDate: '2026-12-10T00:00:00.000Z',
          ticker: 'A',
          title: 'Delta',
          volume24h: '5',
        }),
        makeQuestion({ ticker: 'D', title: 'Beta', volume24h: '5' }),
        makeQuestion({
          expiryDate: '2026-12-01T00:00:00.000Z',
          ticker: 'C',
          title: 'Gamma',
          volume24h: '5',
        }),
        makeQuestion({
          expiryDate: '2026-12-01T00:00:00.000Z',
          ticker: 'G',
          title: 'Alpha',
          volume24h: '5',
        }),
        makeQuestion({
          expiryDate: '2026-12-10T00:00:00.000Z',
          ticker: 'B',
          title: 'Theta',
          volume24h: '10',
        }),
      ],
      NOW
    )

    // B leads on volume; then the volume-5 group by close (Dec 1 before Dec 10, no close last),
    // with the Dec-1 tie broken by title (Alpha before Gamma).
    expect(cards.map(card => card.id)).toEqual(['B', 'G', 'C', 'A', 'D'])
  })

  it('shows a question whose close date cannot be read, without a close date', () => {
    const cards = toQuestionCards(
      [makeQuestion({ expiryDate: 'TBD', ticker: 'X', title: 'No date', volume24h: '1' })],
      NOW
    )

    expect(cards).toHaveLength(1)
    expect(cards[0]).not.toHaveProperty('closesAt')
  })
})
