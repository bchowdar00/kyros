import { createTool } from '@mastra/core/tools'
import type { RustExchangeClient, ApiResult } from '@tiny/api'
import type { RustSeriesInfo, RustSignalCatalogue } from '@tiny/domain'
import * as z from 'zod/mini'

// Server code must never import a `'use client'` module: Next hands it client references,
// and calling one throws at request time. `resolve/*` is the plain half of the chat feature.
import { quotedField, refuseNonBtc } from '@/features/chat/resolve/series'
import { isNumericField } from '@/features/signals/signal-value-format'
import {
  BookSpecSchema,
  CandlesSpecSchema,
  ChartSpecSchema,
  CodeSpecSchema,
  DiffSpecSchema,
  FillsSpecSchema,
  MarkdownSpecSchema,
  MarketSpecSchema,
  parseWidgetSpec,
  PickerSpecSchema,
  PositionSpecSchema,
  RoundsSpecSchema,
  ScenarioSpecSchema,
  SourcesSpecSchema,
  StatGridSpecSchema,
  TableSpecSchema,
  TicketSpecSchema,
  type WidgetSpec,
} from '@/features/chat/specs'
import { exchangeClient, resolveSeries, SeriesInputSchema } from '../shared/exchange'

/** The catalogue and the series list change slowly; a turn's tool calls share one read. */
const CACHE_TTL_MS = 30_000
const SUGGESTIONS_MAX = 5

type ToolContext = Parameters<typeof resolveSeries>[1] & { abortSignal?: AbortSignal | undefined }

type Refusal = { ok: false; reason: string; suggestions?: string[] }
type Acceptance = { ok: true; spec: WidgetSpec }

/**
 * Tool inputs are one flat object per tool, since the tool runtime types only object
 * schemas: `kind` picks the widget, the series is optional and filled from the page, and
 * the fields of the other kinds are optional. The strict spec schema runs in `accept`.
 */
const optional = <Schema extends z.ZodMiniType>(schema: Schema) => z.optional(schema)
const chart = ChartSpecSchema.shape
const candles = CandlesSpecSchema.shape
const book = BookSpecSchema.shape
const rounds = RoundsSpecSchema.shape
const fills = FillsSpecSchema.shape
const markdown = MarkdownSpecSchema.shape
const code = CodeSpecSchema.shape
const sources = SourcesSpecSchema.shape
const diff = DiffSpecSchema.shape
const table = TableSpecSchema.shape
const statGrid = StatGridSpecSchema.shape
const ticket = TicketSpecSchema.shape
const picker = PickerSpecSchema.shape
const scenario = ScenarioSpecSchema.shape

const ChartInput = z.object({
  kind: z.enum(['chart', 'candles']),
  series: SeriesInputSchema,
  title: chart.title,
  refs: optional(chart.refs),
  window_ms: chart.window_ms,
  show_strike: chart.show_strike,
  mark_open: chart.mark_open,
  bucket_ms: candles.bucket_ms,
})
const BookInput = z.object({
  kind: z.literal('book'),
  series: SeriesInputSchema,
  title: book.title,
  depth: book.depth,
})
const MarketInput = z.object({
  kind: z.enum(['market', 'stats']),
  series: SeriesInputSchema,
  title: MarketSpecSchema.shape.title,
})
const PositionInput = z.object({
  kind: z.literal('position'),
  series: SeriesInputSchema,
  title: PositionSpecSchema.shape.title,
})
const TradesInput = z.object({
  kind: z.literal('fills'),
  series: SeriesInputSchema,
  title: fills.title,
  scope: fills.scope,
  limit: fills.limit,
})
const RoundsInput = z.object({
  kind: z.enum(['rounds', 'distribution']),
  series: SeriesInputSchema,
  title: rounds.title,
  count: rounds.count,
})
const ContentInput = z.object({
  kind: z.enum(['markdown', 'code', 'sources', 'diff', 'table', 'stat-grid']),
  title: markdown.title,
  text: optional(markdown.text),
  language: code.language,
  code: optional(code.code),
  sources: optional(sources.sources),
  from: optional(diff.from),
  to: optional(diff.to),
  lines: optional(diff.lines),
  columns: optional(table.columns),
  rows: optional(table.rows),
  stats: optional(statGrid.stats),
})
const AskInput = z.object({
  kind: z.enum(['picker', 'ticket', 'scenario']),
  series: SeriesInputSchema,
  title: ticket.title,
  prompt: optional(picker.prompt),
  options: optional(picker.options),
  default_option_id: picker.default_option_id,
  confirm_label: picker.confirm_label,
  side: optional(ticket.side),
  amount_cents: ticket.amount_cents,
  variable: optional(scenario.variable),
  stake_cents: scenario.stake_cents,
})

function refuse(reason: string, suggestions?: string[]): Refusal {
  return { ok: false, reason, ...(suggestions === undefined ? {} : { suggestions }) }
}

function createCatalogueCache(client: RustExchangeClient) {
  const catalogues = new Map<
    string,
    { at: number; value: Promise<ApiResult<RustSignalCatalogue>> }
  >()
  let seriesList: { at: number; value: Promise<ApiResult<RustSeriesInfo[]>> } | undefined
  return {
    catalogue(series: string, signal: AbortSignal | undefined) {
      const cached = catalogues.get(series)
      if (cached !== undefined && Date.now() - cached.at < CACHE_TTL_MS) return cached.value
      const value = client.getSignals(series, signal)
      catalogues.set(series, { at: Date.now(), value })
      return value
    },
    series(signal: AbortSignal | undefined) {
      if (seriesList !== undefined && Date.now() - seriesList.at < CACHE_TTL_MS)
        return seriesList.value
      const value = client.listSeries(signal)
      seriesList = { at: Date.now(), value }
      return value
    },
  }
}

/** Signal ids in the catalogue that share a word with a refused id, for the model to retry. */
function similarIds(catalogue: RustSignalCatalogue, id: string): string[] {
  const words = id.split(/[.\-_]/).filter(word => word.length > 2 && !/^v\d+$/.test(word))
  return catalogue.signals
    .filter(signal => words.some(word => signal.signal_id.includes(word)))
    .map(signal => signal.signal_id)
    .slice(0, SUGGESTIONS_MAX)
}

export function createWidgetTools(baseUrl: string | undefined) {
  const client = baseUrl === undefined ? undefined : exchangeClient(baseUrl)
  const cache = client === undefined ? undefined : createCatalogueCache(client)

  /** Fill the series, validate the spec, and check it names things the exchange runs. */
  async function accept(
    input: unknown,
    context: ToolContext | undefined
  ): Promise<Acceptance | Refusal> {
    const withSeries =
      typeof input === 'object' && input !== null && needsSeries(input)
        ? { ...input, series: resolveSeries(seriesOf(input), context) }
        : input
    const spec = parseWidgetSpec(withSeries)
    if (spec === undefined)
      return refuse('The widget description is not valid; check the fields and their bounds.')
    if (!('series' in spec)) return { ok: true, spec }
    if (cache === undefined)
      return refuse('The exchange is not configured, so nothing live can be shown.')
    const series = await cache.series(context?.abortSignal)
    if (!series.ok) return refuse('The exchange did not answer.')
    const entry = series.data.find(candidate => candidate.series === spec.series)
    if (entry === undefined)
      return refuse(
        `${spec.series} is not a series the exchange runs.`,
        series.data.map(candidate => candidate.series)
      )
    if (spec.kind === 'candles' || spec.kind === 'market') {
      const nonBtc = refuseNonBtc({ type: 'spot' }, entry.underlying)
      if (nonBtc !== undefined) return refuse(nonBtc, ['chart', 'stats'])
    }
    if (spec.kind !== 'chart') return { ok: true, spec }
    const catalogue = await cache.catalogue(spec.series, context?.abortSignal)
    if (!catalogue.ok) return refuse('The signal catalogue could not be read.')
    for (const ref of spec.refs) {
      const nonBtc = refuseNonBtc(ref, entry.underlying)
      if (nonBtc !== undefined) return refuse(nonBtc, similarIds(catalogue.data, 'score winprob'))
      if (ref.type === 'market' && ref.measure === 'up') {
        const quoted = quotedField(catalogue.data, spec.series)
        // The catalogue is the authority on what the quoted side is called.
        if (quoted === 'up' && entry.underlying !== undefined && entry.underlying !== 'btc')
          return refuse(`${spec.series} does not publish its market price yet.`)
      }
      if (ref.type !== 'signal') continue
      const signal = catalogue.data.signals.find(candidate => candidate.signal_id === ref.id)
      if (signal === undefined)
        return refuse(
          `${ref.id} is not a signal of ${spec.series}.`,
          similarIds(catalogue.data, ref.id)
        )
      const field = signal.fields.find(candidate => candidate.name === ref.field)
      if (field === undefined)
        return refuse(
          `${signal.name} has no field named ${ref.field}.`,
          signal.fields
            .filter(isNumericField)
            .map(candidate => candidate.name)
            .slice(0, SUGGESTIONS_MAX)
        )
      if (!isNumericField(field))
        return refuse(`${ref.field} is a state, not a number, so it cannot be charted.`)
      if (!signal.live) return refuse(`${signal.name} is offline right now.`)
    }
    return { ok: true, spec }
  }

  const showChart = createTool({
    id: 'show-chart',
    description:
      'Draw a live chart in the reply: the default answer to any question about what a signal, a price or a market measure is doing, even when the user did not ask for a chart. kind "chart": one to four references on a time axis over window_ms (default four minutes): a signal field {type:"signal", id, field} from search_signals (on an NBA series that is how you chart the scores, the margin and the model win probability: two fields of one signal are two references), a market measure {type:"market", measure:"up"|"bid"|"ask"|"last"} where "up" is the quoted side whatever the series calls it (Up on BTC, Home on NBA), and on BTC series only the BTC price {type:"spot"} or {type:"strike"}; set show_strike with spot to shade the winning side, mark_open to mark the round open. kind "candles": BTC spot as candles with bucket_ms, BTC series only. The chart keeps updating after you answer; never restate its values as facts you computed.',
    inputSchema: ChartInput,
    execute: (input, context) => accept(input, context),
  })

  const showBook = createTool({
    id: 'show-book',
    description:
      'Draw the live YES order book as a ladder with depth levels a side (default 7, up to 153). Use when asked to see the book, depth, spread or liquidity.',
    inputSchema: BookInput,
    execute: (input, context) => accept(input, context),
  })

  const showMarket = createTool({
    id: 'show-market',
    description:
      'Draw the round at a glance. kind "market": spot against the strike with YES, spread, clock, volume and open interest (BTC series only; on an NBA series chart the scores and the model instead). kind "stats": the round’s volume, open interest, collateral, trades, traders and largest trade, on every series. Both stay live.',
    inputSchema: MarketInput,
    execute: (input, context) => accept(input, context),
  })

  const showPosition = createTool({
    id: 'show-position',
    description:
      'Draw the user’s live position card for a series: signed lots, entry, mark, locked collateral and the round clock. Needs the user to be signed in.',
    inputSchema: PositionInput,
    execute: (input, context) => accept(input, context),
  })

  const showTrades = createTool({
    id: 'show-trades',
    description:
      'Draw a live fill log: scope "mine" for the user’s own fills in the series, "tape" for everyone’s recent fills. limit rows (default 8).',
    inputSchema: TradesInput,
    execute: (input, context) => accept(input, context),
  })

  const showRounds = createTool({
    id: 'show-rounds',
    description:
      'Draw recent rounds. kind "rounds": a strip of up and down outcomes with the streak (count, default 12). kind "distribution": a histogram of the moves from strike over recent rounds with the open round pinned.',
    inputSchema: RoundsInput,
    execute: (input, context) => accept(input, context),
  })

  const showContent = createTool({
    id: 'show-content',
    description:
      'Place formatted content in the reply that you wrote yourself: kind "markdown" (headings, lists, emphasis), "code" (with language), "sources" (title and url per source), "diff" (from, to, context/added/removed lines), "table" (columns with id and label; each row is an array of cells in column order, text or number) or "stat-grid" (label, value, optional delta and direction). The frame says the numbers are yours, not live; for live numbers use the other show tools.',
    inputSchema: ContentInput,
    execute: (input, context) => accept(input, context),
  })

  const showAsk = createTool({
    id: 'show-ask',
    description:
      'Put a choice in front of the user. kind "picker": a prompt with two to six options; their choice comes back as their next message. kind "ticket": a YES/NO order form with live prices and a stake in cents; confirming comes back as a message but places no order. kind "scenario": a slider with variable "btc_move" or "stake" and readouts that follow it.',
    inputSchema: AskInput,
    execute: (input, context) => accept(input, context),
  })

  return {
    showChart,
    showBook,
    showMarket,
    showPosition,
    showTrades,
    showRounds,
    showContent,
    showAsk,
  }
}

/** Kinds whose spec names a series; the rest carry their data inline. */
const INLINE_KINDS = new Set([
  'markdown',
  'code',
  'sources',
  'diff',
  'table',
  'stat-grid',
  'picker',
])

function needsSeries(input: object): boolean {
  const kind = 'kind' in input ? input.kind : undefined
  return typeof kind === 'string' && !INLINE_KINDS.has(kind)
}

function seriesOf(input: object): string | undefined {
  const series = 'series' in input ? input.series : undefined
  return typeof series === 'string' ? series : undefined
}
