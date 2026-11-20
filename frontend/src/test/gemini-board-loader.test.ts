// @vitest-environment node
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import fixture from '@/features/home/gemini-board.fixture.json'
import { createGeminiBoardLoader } from '@/features/home/gemini-board-loader'

const GEMINI = 'https://gemini.test'
const EVENTS = `${GEMINI}/v1/prediction-markets/events`
const CATEGORIES = `${GEMINI}/v1/prediction-markets/categories`
const NOW = Date.parse('2026-11-20T08:17:00Z')

// Gemini's own list of categories, as its endpoint gives it.
const CATEGORY_LIST = [
  'Live',
  'Sports',
  'College Football',
  'Pro Football',
  'Crypto',
  'Commodities',
  'Economics',
  'Economy',
  'Fun/Culture',
  'Media',
  'Politics',
  'Tech',
  'Business',
  'Weather',
  'Culture',
  'Other',
  'Finance',
]

const question = (ticker: string, category: string, volume24h: string) => ({
  category,
  contracts: [
    { label: 'Yes', marketState: 'open', prices: { buy: { yes: '0.50' } }, status: 'active' },
  ],
  expiryDate: '2026-12-01T00:00:00.000Z',
  status: 'active',
  ticker,
  title: ticker,
  type: 'binary',
  volume24h,
})

// A crypto market that is not an up/down round: it must become a question card from the
// list the up/down cards already read, with no second crypto request.
const cryptoQuestion = question('BTCRANGE', 'Crypto', '3')

const questionsByCategory: Record<string, ReturnType<typeof question>[]> = {
  Commodities: [question('CMD1', 'Commodities', '7')],
  Economics: [question('ECO1', 'Economics', '4')],
  Politics: [question('POL1', 'Politics', '20')],
  Tech: [question('TECH1', 'Tech', '50')],
  // Five at a page size of three forces the loader to follow two pages.
  Weather: Array.from({ length: 5 }, (_, index) =>
    question(`WX${index}`, 'Weather', String(index))
  ),
}

const server = setupServer()
const asked: string[] = []
let categoriesAsked = 0

function page(all: readonly unknown[], params: URLSearchParams) {
  const offset = Number(params.get('offset') ?? 0)
  const limit = Number(params.get('limit'))
  return HttpResponse.json({
    data: all.slice(offset, offset + limit),
    pagination: { total: all.length },
  })
}

function mockGemini({
  categories = true,
  failCategory,
  sports = true,
}: {
  categories?: boolean
  failCategory?: string
  sports?: boolean
} = {}): void {
  server.use(
    http.get(CATEGORIES, () => {
      categoriesAsked += 1
      return categories
        ? HttpResponse.json({ categories: CATEGORY_LIST })
        : HttpResponse.json({ message: 'down' }, { status: 503 })
    }),
    http.get(EVENTS, ({ request }) => {
      const params = new URL(request.url).searchParams
      asked.push(params.toString())
      if (params.get('status') !== 'active') return HttpResponse.json({ data: [] })
      const category = params.get('category')
      if (category === 'crypto') {
        return HttpResponse.json({
          data: [...fixture.crypto, cryptoQuestion],
          pagination: { total: fixture.crypto.length + 1 },
        })
      }
      if (category === 'sports') {
        return sports
          ? page(fixture.games, params)
          : HttpResponse.json({ message: 'down' }, { status: 503 })
      }
      if (category === failCategory) return HttpResponse.json({ message: 'down' }, { status: 503 })
      return page(questionsByCategory[category ?? ''] ?? [], params)
    })
  )
}

function loader(now: () => number = () => NOW) {
  return createGeminiBoardLoader({ apiUrl: GEMINI, now, pageSize: 3 })
}

const categoriesOf = () =>
  asked.map(query => new URLSearchParams(query).get('category')).filter(name => name !== null)

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  asked.length = 0
  categoriesAsked = 0
})
afterAll(() => server.close())

describe('the Gemini board loader', () => {
  it('reads the up/down list and every page of the game list', async () => {
    mockGemini()

    const board = await loader()()

    expect(board?.upDown.map(card => card.id)).toEqual(['BTC05M', 'ETH15M'])
    expect(board?.games.map(card => card.id)).toEqual([
      'LALIGA-2611202000-ESY-ATH-M',
      'NBA-2611210000-CHA-ATL-M',
      'NBA-2611210030-TOR-DET-M',
    ])
    expect(board?.asOf).toBe('2026-11-20T08:17:00.000Z')
    // Only the winner market of each game is asked for, a page at a time.
    const games = asked.filter(query => query.includes('category=sports'))
    expect(games).toHaveLength(2)
    expect(games.every(query => query.includes('sports_market_type=moneyline'))).toBe(true)
  })

  it('gives the time the list was read, not the time the page was asked for', async () => {
    mockGemini()
    let clock = NOW
    const load = loader(() => clock)
    await load()

    clock += 10_000
    expect((await load())?.asOf).toBe('2026-11-20T08:17:00.000Z')
  })

  it('reads the up/down prices again before showing ones more than a minute old', async () => {
    mockGemini()
    let clock = NOW
    const load = loader(() => clock)
    expect((await load())?.upDown).toHaveLength(2)

    // Gemini now lists no rounds. A visitor a minute later must see that, not the old prices.
    server.use(
      http.get(EVENTS, ({ request }) =>
        new URL(request.url).searchParams.get('category') === 'crypto'
          ? HttpResponse.json({ data: [] })
          : undefined
      )
    )
    clock += 61_000

    expect((await load())?.upDown).toEqual([])
  })

  it('reads the categories, then every non-excluded category, and the crypto list only once', async () => {
    mockGemini()

    const board = await loader()()

    expect(categoriesAsked).toBe(1)
    const categories = categoriesOf()
    // Live and the football feeds are never asked for.
    expect(categories).not.toContain('Live')
    expect(categories).not.toContain('College Football')
    expect(categories).not.toContain('Pro Football')
    // Crypto is read once, by the up/down loader, and not again for the questions.
    expect(categories.filter(name => name === 'crypto')).toHaveLength(1)
    // A category with more than a page is followed to the end.
    expect(categories.filter(name => name === 'Weather')).toHaveLength(2)
    // The crypto question comes from the already-read crypto list.
    expect(board?.questions.map(card => card.id)).toContain('BTCRANGE')
    expect(board?.questions.map(card => card.id)).toEqual(
      expect.arrayContaining(['POL1', 'TECH1', 'WX0'])
    )
  })

  it('falls back to a fixed list of categories when the categories read fails', async () => {
    mockGemini({ categories: false })

    const board = await loader()()

    const categories = categoriesOf()
    for (const name of [
      'Politics',
      'Economics',
      'Weather',
      'Commodities',
      'Tech',
      'Culture',
      'Business',
    ]) {
      expect(categories).toContain(name)
    }
    // The categories only in Gemini's full answer are not reached from the fallback.
    expect(categories).not.toContain('Economy')
    expect(categories).not.toContain('Media')
    expect(board?.questions.map(card => card.id)).toContain('POL1')
  })

  it('still shows the other question categories when one of them fails', async () => {
    mockGemini({ failCategory: 'Politics' })

    const board = await loader()()

    const ids = board?.questions.map(card => card.id) ?? []
    expect(ids).not.toContain('POL1')
    expect(ids).toEqual(expect.arrayContaining(['TECH1', 'WX0', 'BTCRANGE']))
  })

  it('keeps a category it has read before when that category later fails', async () => {
    let at = NOW
    const load = loader(() => at)
    mockGemini()
    const first = await load()
    expect(first?.questions.map(card => card.id)).toContain('POL1')

    // Past the questions' fresh window: Gemini is asked again, and Politics is down.
    server.resetHandlers()
    mockGemini({ failCategory: 'Politics' })
    at += 6 * 60 * 1000
    // The first call past the window answers from what is kept and starts the new read.
    await load()
    await new Promise(resolve => setTimeout(resolve, 150))
    const later = await load()
    expect(categoriesOf().filter(name => name === 'Politics').length).toBeGreaterThan(1)

    expect(later?.questions.map(card => card.id)).toEqual(
      expect.arrayContaining(['POL1', 'TECH1', 'WX0'])
    )
  })

  it('asks again soon, not minutes later, when every question category fails at once', async () => {
    let at = NOW
    const load = loader(() => at)
    mockGemini()
    await load()

    // Past the fresh window, with every question category down.
    server.resetHandlers()
    mockGemini()
    server.use(
      http.get(EVENTS, ({ request }) => {
        const category = new URL(request.url).searchParams.get('category')
        if (category === null || category === 'crypto' || category === 'sports') return undefined
        asked.push(new URL(request.url).searchParams.toString())
        return HttpResponse.json({ message: 'down' }, { status: 503 })
      })
    )
    at += 6 * 60 * 1000
    await load()
    await new Promise(resolve => setTimeout(resolve, 150))
    const afterOutage = categoriesOf().filter(name => name === 'Politics').length

    // Twenty seconds on: the outage is asked about again, and what was kept still shows.
    at += 20_000
    const later = await load()
    await new Promise(resolve => setTimeout(resolve, 150))

    expect(categoriesOf().filter(name => name === 'Politics').length).toBeGreaterThan(afterOutage)
    expect(later?.questions.map(card => card.id)).toEqual(
      expect.arrayContaining(['POL1', 'TECH1', 'WX0'])
    )
  })

  it('reads the question categories side by side, not one after another', async () => {
    mockGemini()
    let waiting = 0
    let most = 0
    server.use(
      http.get(EVENTS, async ({ request }) => {
        const category = new URL(request.url).searchParams.get('category')
        if (category === null || !CATEGORY_LIST.includes(category)) return undefined
        waiting += 1
        most = Math.max(most, waiting)
        await new Promise(resolve => setTimeout(resolve, 20))
        waiting -= 1
        return undefined
      })
    )

    await loader()()

    expect(most).toBeGreaterThan(1)
  })

  it('shows the rounds and games without waiting for a slow questions read', async () => {
    mockGemini()
    server.use(
      http.get(CATEGORIES, async () => {
        await new Promise(resolve => setTimeout(resolve, 400))
        return HttpResponse.json({ categories: CATEGORY_LIST })
      })
    )

    const board = await createGeminiBoardLoader({
      apiUrl: GEMINI,
      now: () => NOW,
      pageSize: 3,
      questionsWaitMs: 50,
    })()

    expect(board?.upDown.length).toBeGreaterThan(0)
    expect(board?.games.length).toBeGreaterThan(0)
    // Only the crypto question, which rides on the up/down read, is there yet.
    expect(board?.questions.map(card => card.id)).toEqual(['BTCRANGE'])
  })

  it('builds the crypto questions from the list already read for the up/down cards', async () => {
    mockGemini()

    const board = await loader()()

    expect(board?.questions.map(card => card.id)).toContain('BTCRANGE')
    expect(categoriesOf().filter(name => name === 'crypto')).toHaveLength(1)
  })

  it('still shows the up/down markets when the game list fails', async () => {
    mockGemini({ sports: false })

    const board = await loader()()

    expect(board?.upDown).toHaveLength(2)
    expect(board?.games).toEqual([])
  })

  it('has no board when Gemini does not answer at all', async () => {
    server.use(
      http.get(CATEGORIES, () => HttpResponse.error()),
      http.get(EVENTS, () => HttpResponse.error())
    )

    expect(await loader()()).toBeUndefined()
  })

  it('never asks Gemini for anything but its public market list', async () => {
    mockGemini()
    await loader()()

    for (const query of asked) {
      expect(query).not.toMatch(/order|position|balance|terms/)
    }
  })
})
