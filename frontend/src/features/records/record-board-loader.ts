import { RustRecordViewSchema } from '@kyros/domain'
import * as z from 'zod/mini'

import { decodeRecordLog, ExplorerLogSchema, type RecordEvent } from './arc-logs'
import { buildRecordBoard, type RecordBoard, type RecordBoardRow } from './board'

/** A board row with what the exchange adds while it still holds the deployment. */
export type NamedRecordBoardRow = RecordBoardRow & {
  /** The strategy's name; unknown once the exchange no longer holds the deployment. */
  name: string | undefined
  /** The market series it trades (`btc-5m`); unknown for the same reason. */
  series: string | undefined
}

export type RecordBoardView =
  | {
      status: 'ok'
      /** When the logs were read, ISO 8601. */
      asOf: string
      contract: string
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
      const rows = await Promise.all(
        board.rows.map(async (row, index): Promise<NamedRecordBoardRow> => {
          const described =
            exchangeUrl === undefined || index >= NAMED_ROWS
              ? { name: undefined, series: undefined }
              : await describeRow(exchangeUrl, row.deploymentId, deadline)
          return { ...row, ...described }
        })
      )

      return {
        asOf: new Date(readAt).toISOString(),
        contract: options.contract,
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
