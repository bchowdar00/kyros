import { RustRecordViewSchema } from '@kyros/domain'
import * as z from 'zod/mini'

import {
  cardOf,
  cardSideTitle,
  type StrategyCardName,
  toStrategyCardNames,
} from '@/features/market-detail/strategy-card-names'
import { decodeRecordLog, ExplorerLogSchema, type RecordEvent } from './arc-logs'
import { buildRecordBoard, type RecentSeal, type RecordBoard, type RecordBoardRow } from './board'

/** A board row with what the exchange adds while it still holds the deployment. */
export type NamedRecordBoardRow = RecordBoardRow & {
  /**
   * The public card this strategy runs from and which side of it, so anyone can run the same
   * one; absent for a strategy a visitor wrote, which only its writer holds.
   */
  copy?: { inverse: boolean; title: string } | undefined
  /** The strategy's name; unknown once the exchange no longer holds the deployment. */
  name: string | undefined
  /** The market series it trades (`btc-5m`); unknown for the same reason. */
  series: string | undefined
}

/** A sealed round with the name and market of the strategy that sealed it, when known. */
export type NamedRecentSeal = RecentSeal & {
  name: string | undefined
  series: string | undefined
}

export type RecordBoardView =
  | {
      status: 'ok'
      /** When the logs were read, ISO 8601. */
      asOf: string
      contract: string
      /** The newest sealed rounds of any strategy, newest first. */
      recent: NamedRecentSeal[]
      rows: NamedRecordBoardRow[]
      totals: RecordBoard['totals']
      /** True when the contract has more logs than were read, so older rounds are missing. */
      truncated: boolean
    }
  | { status: 'unavailable' }

export interface RecordBoardLoaderOptions {
  /** The `StrategyRecord` contract's address. */
  contract: string
  /** Exchange origin for names; without it the board shows ids only. */
  exchangeUrl: string | undefined
  /** Block explorer origin (Blockscout) whose log list is the board's source. */
  explorerUrl: string
  /** Explorer pages to read at most (50 logs each). */
  maxPages?: number
  now?: () => number
}

const DEFAULT_MAX_PAGES = 40
const FRESH_FOR_MS = 30_000
/** How long a failed read stands, so a struggling explorer is not asked again by every visitor. */
const FAILED_FOR_MS = 5000
const REQUEST_TIMEOUT_MS = 8000
/** The whole read, every page and name included, gives up after this long. */
const READ_TIMEOUT_MS = 20_000
/** Rows that get a name lookup; the rest show their id. */
const NAMED_ROWS = 50

const LogPageSchema = z.object({
  items: z.array(ExplorerLogSchema),
  // The cursor is the explorer's own; whatever it holds is handed back as it came.
  next_page_params: z.nullable(z.record(z.string(), z.unknown())),
})

/** The part of `GET /api/strategies/templates?series=` the card titles come from. */
const CardCatalogueSchema = z.object({
  templates: z.array(
    z.object({
      inverse: z.object({ name: z.string() }),
      primary: z.object({ name: z.string() }),
      title: z.string(),
    })
  ),
})

const AccountStrategySchema = z.object({
  strategy: z.optional(z.nullable(z.object({ deployment_id: z.string(), name: z.string() }))),
})

async function getJson(url: URL | string, deadline: AbortSignal): Promise<unknown> {
  const response = await fetch(url, {
    cache: 'no-store',
    headers: { Accept: 'application/json' },
    signal: AbortSignal.any([deadline, AbortSignal.timeout(REQUEST_TIMEOUT_MS)]),
  })
  if (!response.ok) throw new Error(`${response.status} from ${new URL(url).host}`)
  return response.json()
}

/** Every decodable event of the contract, newest first, and whether the read stopped early. */
async function readEvents(
  options: RecordBoardLoaderOptions,
  deadline: AbortSignal
): Promise<{ events: RecordEvent[]; truncated: boolean }> {
  const events: RecordEvent[] = []
  const maxPages = options.maxPages ?? DEFAULT_MAX_PAGES
  let cursor: Record<string, unknown> | null = {}

  for (let page = 0; page < maxPages && cursor !== null; page += 1) {
    const url = new URL(`api/v2/addresses/${options.contract}/logs`, `${options.explorerUrl}/`)
    for (const [key, value] of Object.entries(cursor)) {
      // An empty cursor field has no query form; the explorer pages without it.
      if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
        url.searchParams.set(key, String(value))
      }
    }

    const parsed = z.parse(LogPageSchema, await getJson(url, deadline))
    for (const log of parsed.items) {
      const event = decodeRecordLog(log)
      if (event !== undefined) events.push(event)
    }
    cursor = parsed.next_page_params
  }

  return { events, truncated: cursor !== null }
}

/** The name and series of one deployment, or nothing the exchange can vouch for. */
async function describeRow(
  exchangeUrl: string,
  deploymentId: string,
  deadline: AbortSignal
): Promise<Pick<NamedRecordBoardRow, 'name' | 'series'>> {
  try {
    const record = z.parse(
      RustRecordViewSchema,
      await getJson(new URL(`api/records/${deploymentId}`, `${exchangeUrl}/`), deadline)
    )
    const account = z.safeParse(
      AccountStrategySchema,
      await getJson(new URL(`api/accounts/${record.account}`, `${exchangeUrl}/`), deadline).catch(
        () => undefined
      )
    )
    // The account may run a newer deployment by now; only this deployment's own name counts.
    const strategy = account.success ? account.data.strategy : undefined
    const name =
      strategy !== undefined && strategy !== null && strategy.deployment_id === record.deployment
        ? strategy.name
        : undefined

    return { name, series: record.series }
  } catch {
    return { name: undefined, series: undefined }
  }
}

/** The public cards of one market, or none when the exchange cannot say. */
async function cardsOf(
  exchangeUrl: string,
  series: string,
  deadline: AbortSignal
): Promise<StrategyCardName[]> {
  try {
    const url = new URL('api/strategies/templates', `${exchangeUrl}/`)
    url.searchParams.set('series', series)
    return toStrategyCardNames(z.parse(CardCatalogueSchema, await getJson(url, deadline)))
  } catch {
    return []
  }
}

/**
 * A strategy run from a public card is shown under the card's title, as on the market page.
 * One read of the cards per market that has a named row.
 */
async function withCardTitles(
  exchangeUrl: string | undefined,
  rows: NamedRecordBoardRow[],
  deadline: AbortSignal
): Promise<NamedRecordBoardRow[]> {
  if (exchangeUrl === undefined) return rows
  const markets = new Set<string>()
  for (const row of rows) {
    if (row.name !== undefined && row.series !== undefined) markets.add(row.series)
  }
  const cards = new Map(
    await Promise.all(
      [...markets].map(
        async series => [series, await cardsOf(exchangeUrl, series, deadline)] as const
      )
    )
  )

  return rows.map(row => {
    if (row.name === undefined || row.series === undefined) return row
    const card = cardOf(row.name, cards.get(row.series) ?? [])
    return card === undefined
      ? row
      : { ...row, copy: card, name: cardSideTitle(card.title, card.inverse) }
  })
}

/**
 * A loader for the public record board. The board is built from the contract's logs on Arc,
 * read through the explorer, so it is the same whatever the exchange remembers; the exchange
 * only adds names. A good board is fresh for thirty seconds. After that a visitor still gets
 * it at once, with the time it was read, while a new read runs behind it; a read that fails
 * never replaces a good board, and the explorer is then left alone for five seconds. Only a
 * visitor who arrives before any good read waits for one. Visitors share a running read, which
 * is why it takes no caller's abort signal: one visitor leaving must not fail another's page.
 */
export function createRecordBoardLoader(
  options: RecordBoardLoaderOptions
): () => Promise<RecordBoardView> {
  const now = options.now ?? Date.now
  let good: { at: number; view: RecordBoardView } | undefined
  let failedAt: number | undefined
  let reading: Promise<RecordBoardView> | undefined

  async function read(): Promise<RecordBoardView> {
    const readAt = now()
    try {
      const deadline = AbortSignal.timeout(READ_TIMEOUT_MS)
      const { events, truncated } = await readEvents(options, deadline)
      const board = buildRecordBoard(events)
      const { exchangeUrl } = options
      const described = await Promise.all(
        board.rows.map(async (row, index): Promise<NamedRecordBoardRow> => {
          const found =
            exchangeUrl === undefined || index >= NAMED_ROWS
              ? { name: undefined, series: undefined }
              : await describeRow(exchangeUrl, row.deploymentId, deadline)
          return { ...row, ...found }
        })
      )
      const rows = await withCardTitles(exchangeUrl, described, deadline)
      const byId = new Map(rows.map(row => [row.deploymentId, row]))
      const recent = board.recent.map((seal): NamedRecentSeal => {
        const row = byId.get(seal.deploymentId)
        return { ...seal, name: row?.name, series: row?.series }
      })

      return {
        asOf: new Date(readAt).toISOString(),
        contract: options.contract,
        recent,
        rows,
        status: 'ok',
        totals: board.totals,
        truncated,
      }
    } catch {
      return { status: 'unavailable' }
    }
  }

  function refresh(): Promise<RecordBoardView> {
    reading ??= read().then(view => {
      if (view.status === 'ok') {
        good = { at: now(), view }
        failedAt = undefined
      } else {
        failedAt = now()
      }
      reading = undefined
      return good?.view ?? view
    })
    return reading
  }

  return () => {
    const failedLately = failedAt !== undefined && now() - failedAt < FAILED_FOR_MS
    if (good !== undefined) {
      if (now() - good.at >= FRESH_FOR_MS && !failedLately) void refresh()
      return Promise.resolve(good.view)
    }
    return failedLately ? Promise.resolve({ status: 'unavailable' }) : refresh()
  }
}
