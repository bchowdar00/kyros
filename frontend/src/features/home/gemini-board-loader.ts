import * as z from 'zod/mini'

import { createKeptLoader } from '@/lib/kept-loader'
import { withinMs } from '@/lib/within-ms'

import {
  type GameCard,
  type QuestionCard,
  toGameCards,
  toQuestionCards,
  toUpDownCards,
  type UpDownCard,
} from './gemini-board'

/** Gemini's markets as the Markets page lists them, and when they were read. */
export interface GeminiBoard {
  asOf: string
  games: GameCard[]
  questions: QuestionCard[]
  upDown: UpDownCard[]
}

export interface GeminiBoardLoaderOptions {
  /** Gemini's public REST origin. */
  apiUrl: string
  now?: () => number
  /** Games asked for per request; a page of a hundred is about ten megabytes. */
  pageSize?: number
  /** How long a caller waits for the questions before the board goes out without them. */
  questionsWaitMs?: number
}

const LIST_PATH = 'v1/prediction-markets/events'
const CATEGORIES_PATH = 'v1/prediction-markets/categories'
/** Categories the questions read skips: up/down crypto and the games are read on their own. */
const SKIP_CATEGORIES = new Set(['Live', 'Sports', 'College Football', 'Pro Football', 'Crypto'])
/** Used when Gemini's category list cannot be read; the ones that carry question markets. */
const FALLBACK_CATEGORIES = [
  'Politics',
  'Economics',
  'Weather',
  'Commodities',
  'Tech',
  'Culture',
  'Business',
]
const DEFAULT_PAGE_SIZE = 100
/** More pages than Gemini has games means its paging is not what this code expects. */
const MAX_PAGES = 12
const REQUEST_TIMEOUT_MS = 15_000
/** Round prices move by the second; the page's own three cards are live, these are a glance. */
const UP_DOWN_FRESH_MS = 5000
/** Up/down prices older than this are not shown; the visitor waits for a new read. */
const UP_DOWN_STALE_MS = 60_000
/** The game list is large and its prices move slowly before tip-off. */
const GAMES_FRESH_MS = 5 * 60 * 1000
const FAILED_FOR_MS = 15_000
/**
 * The questions are a dozen reads; a first visitor gets the rounds and games without them
 * if they take longer than this, and the questions on a later refresh.
 */
const QUESTIONS_WAIT_MS = 1500

const ListSchema = z.object({
  data: z.array(z.unknown()),
  pagination: z.optional(z.object({ total: z.number() })),
})

const CategoriesSchema = z.object({ categories: z.array(z.string()) })

async function requestJson(url: URL): Promise<unknown> {
  const response = await fetch(url, {
    cache: 'no-store',
    headers: { Accept: 'application/json' },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`${response.status} from ${url.host}`)
  return response.json()
}

async function list(apiUrl: string, query: Record<string, string>) {
  const url = new URL(LIST_PATH, `${apiUrl}/`)
  for (const [key, value] of Object.entries(query)) url.searchParams.set(key, value)
  return z.parse(ListSchema, await requestJson(url))
}

async function categoryNames(apiUrl: string): Promise<string[]> {
  const url = new URL(CATEGORIES_PATH, `${apiUrl}/`)
  return z.parse(CategoriesSchema, await requestJson(url)).categories
}

/** Every entry of a listing, following `pagination` one page at a time. */
async function allEntries(
  apiUrl: string,
  pageSize: number,
  query: Record<string, string>
): Promise<unknown[]> {
  const entries: unknown[] = []
  let total = Number.POSITIVE_INFINITY
  for (let page = 0; page < MAX_PAGES && page * pageSize < total; page += 1) {
    const body = await list(apiUrl, {
      ...query,
      limit: String(pageSize),
      offset: String(page * pageSize),
    })
    entries.push(...body.data)
    total = body.pagination?.total ?? 0
    if (body.data.length === 0) break
  }
  return entries
}

/**
 * A loader for Gemini's public market list: every up/down series and every game's winner
 * market. Read-only and public; nothing here touches an account. The two lists are kept
 * separately, so one failing leaves the other on the page, and a visitor is never made to
 * wait on Gemini once a first read has succeeded.
 */
export function createGeminiBoardLoader(
  options: GeminiBoardLoaderOptions
): () => Promise<GeminiBoard | undefined> {
  const now = options.now ?? Date.now
  const pageSize = options.pageSize ?? DEFAULT_PAGE_SIZE

  // The raw up/down entries are kept, not the cards: which round is open depends on the time
  // the page is asked for, not the time the list was read.
  const upDownEntries = createKeptLoader(
    async () => {
      const readAt = now()
      const body = await list(options.apiUrl, {
        category: 'crypto',
        limit: '500',
        status: 'active',
      })
      return { entries: body.data, readAt }
    },
    // A round lasts five minutes, so a price from the round before is worse than none.
    {
      failedForMs: FAILED_FOR_MS,
      freshForMs: UP_DOWN_FRESH_MS,
      now,
      staleForMs: UP_DOWN_STALE_MS,
    }
  )

  const gameCards = createKeptLoader(
    async () => {
      const readAt = now()
      const games: GameCard[] = []
      let total = Number.POSITIVE_INFINITY
      for (let page = 0; page < MAX_PAGES && page * pageSize < total; page += 1) {
        const body = await list(options.apiUrl, {
          category: 'sports',
          limit: String(pageSize),
          offset: String(page * pageSize),
          sports_market_type: 'moneyline',
          status: 'active',
        })
        // Each page is turned into cards and dropped, so the whole list is never held at once.
        games.push(...toGameCards(body.data, now()))
        total = body.pagination?.total ?? 0
        if (body.data.length === 0) break
      }
      return { games, readAt }
    },
    { failedForMs: FAILED_FOR_MS, freshForMs: GAMES_FRESH_MS, now }
  )

  // The raw question entries across every category that carries them; kept like the games.
  // What each category last answered, so one that fails for a moment keeps its cards.
  const keptByCategory = new Map<string, { entries: readonly unknown[]; readAt: number }>()
  const questionEntries = createKeptLoader(
    async () => {
      const startedAt = now()
      const listed = await categoryNames(options.apiUrl).catch(() => FALLBACK_CATEGORIES)
      const names = listed.filter(name => !SKIP_CATEGORIES.has(name))
      // Side by side: a dozen small lists, and the slowest one sets the wait, not their sum.
      const reads = await Promise.allSettled(
        names.map(category => allEntries(options.apiUrl, pageSize, { category, status: 'active' }))
      )
      const entries: unknown[] = []
      let readAt = startedAt
      let answered = 0
      for (const [index, category] of names.entries()) {
        const read = reads[index]
        if (read?.status === 'fulfilled') {
          keptByCategory.set(category, { entries: read.value, readAt: startedAt })
          answered += 1
        }
        // A category Gemini would not list this time shows what it last listed, if anything.
        const kept = keptByCategory.get(category)
        if (kept === undefined) continue
        entries.push(...kept.entries)
        readAt = Math.min(readAt, kept.readAt)
      }
      // Nothing answered: a failed read, asked again soon, which never replaces a good one.
      if (names.length > 0 && answered === 0) {
        throw new Error('no question category could be read')
      }
      return { entries, readAt }
    },
    { failedForMs: FAILED_FOR_MS, freshForMs: GAMES_FRESH_MS, now }
  )

  return async () => {
    const [upDown, listed, questioned] = await Promise.all([
      upDownEntries(),
      gameCards(),
      withinMs(questionEntries(), options.questionsWaitMs ?? QUESTIONS_WAIT_MS),
    ])
    if (upDown === undefined && listed === undefined) return undefined
    const at = now()
    // The oldest of the reads: the page never claims to be newer than what it shows.
    const readAt = Math.min(upDown?.readAt ?? at, listed?.readAt ?? at, questioned?.readAt ?? at)

    return {
      asOf: new Date(readAt).toISOString(),
      games: (listed?.games ?? [])
        .map(game => ({ ...game, started: Date.parse(game.startsAt) <= at }))
        .toSorted(
          (a, b) => Date.parse(a.startsAt) - Date.parse(b.startsAt) || a.id.localeCompare(b.id)
        ),
      // The crypto list already read for the up/down cards also yields its question cards.
      questions: toQuestionCards([...(questioned?.entries ?? []), ...(upDown?.entries ?? [])], at),
      upDown: upDown === undefined ? [] : toUpDownCards(upDown.entries, at),
    }
  }
}
