// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import fixture from '@/features/home/venue-board.fixture.json'
import { loadVenueBoard } from '@/features/home/venue-board-loader'

const EVENTS = 'https://api.elections.kalshi.com/trade-api/v2/events'

const server = setupServer()
const asked: string[] = []

const seriesAsked = () =>
  asked.map(query => new URLSearchParams(query).get('series_ticker')).filter(name => name !== null)

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  asked.length = 0
})
afterAll(() => server.close())

describe('loadVenueBoard', () => {
  it('reads every league in order, each with the right query, and keeps those with games', async () => {
    server.use(
      http.get(EVENTS, ({ request }) => {
        asked.push(new URL(request.url).search)
        return HttpResponse.json(fixture)
      })
    )

    const board = await loadVenueBoard()

    // Leagues Gemini does not list, checked against its own board on 2026-11-20.
    expect(board?.leagues.map(league => league.label)).toEqual([
      'Bundesliga',
      'Serie A',
      'Ligue 1',
      'MLS',
      'UFC',
    ])
    // Each league carries the games the body parses to.
    expect(board?.leagues[0]?.games.map(game => game.id)).toEqual([
      'KXEPLGAME-26NOV21MCIFUL',
      'KXNFLGAME-26NOV22ARIKC',
      'KXNFLGAME-26NOV22BALCAR',
    ])
    // Every league is asked for, by series ticker.
    expect(seriesAsked().toSorted()).toEqual([
      'KXBUNDESLIGAGAME',
      'KXLIGUE1GAME',
      'KXMLSGAME',
      'KXSERIEAGAME',
      'KXUFCFIGHT',
    ])
    for (const query of asked) {
      const params = new URLSearchParams(query)
      expect(params.get('status')).toBe('open')
      expect(params.get('with_nested_markets')).toBe('true')
    }
  })

  it('leaves out a league whose read fails or yields no games, keeping the rest', async () => {
    server.use(
      http.get(EVENTS, ({ request }) => {
        const series = new URL(request.url).searchParams.get('series_ticker')
        // One rejects, one answers 500, two have no games; only the UFC keeps its games.
        if (series === 'KXBUNDESLIGAGAME') return HttpResponse.error()
        if (series === 'KXSERIEAGAME')
          return HttpResponse.json({ message: 'down' }, { status: 500 })
        if (series === 'KXLIGUE1GAME' || series === 'KXMLSGAME') {
          return HttpResponse.json({ events: [] })
        }
        return HttpResponse.json(fixture)
      })
    )

    const board = await loadVenueBoard()

    expect(board?.leagues.map(league => league.label)).toEqual(['UFC'])
  })

  it('has no board when no league is left', async () => {
    server.use(http.get(EVENTS, () => HttpResponse.error()))

    expect(await loadVenueBoard()).toBeUndefined()
  })
})
