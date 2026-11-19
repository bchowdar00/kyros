import type { RustGeminiSeries } from '@kyros/domain'
import type { Cents } from '@kyros/money'
import type { ChartPoint, TimeRange } from '@kyros/ui/charts/lib/types'
import type { ChartWindow } from '@kyros/ui/charts/chart-window'
import type { RecentMarketResult } from '@kyros/ui/charts/recent-market-results'

export type MarketSide = 'away' | 'home'

export type StrategyStatus = 'active' | 'paused'

export type PublicStrategySortId = 'most-deployed' | 'most-running'

export type StrategyTabId = 'open-orders' | 'strategies' | 'trade-history'

export interface ImageAsset {
  alt: string
  src: string
}

export interface MarketTeam {
  abbreviation: string
  id: string
  logo: ImageAsset
  logoInset?: boolean
  name: string
  side: MarketSide
}

export interface MarketOutcome {
  label: string
  priceCents?: number
  priceTicks?: number
  probability?: number
  side: MarketSide
}

export interface MarketHeaderData {
  outcomes: readonly [MarketOutcome, MarketOutcome]
  surfaceImage: ImageAsset
  subtitle: string
  /** Sports markets only; crypto and other markets have no team identities. */
  teams?: readonly [MarketTeam, MarketTeam]
  title: string
}

export interface ProbabilitySeries {
  label: string
  points: readonly ChartPoint[]
  side: MarketSide
}

export interface MarketProbabilityChartData {
  /** One-line narration of the latest market-moving moment, shown below the chart. */
  commentary?: string
  /** Pulses the latest sample while the game is in progress. */
  live: boolean
  /** Visible window, usually the scheduled game slot. */
  range: TimeRange
  series: readonly [ProbabilitySeries, ProbabilitySeries]
  /** The exchange series slug the `/ws/book` midpoint streams from; absent for static fixtures. */
  seriesSlug?: string
  timeZone: string
}

export interface MarketPriceChartData {
  /** One-line narration of the latest market-moving moment, shown below the chart. */
  commentary?: string
  /** Series label, for example the asset symbol. */
  label: string
  /** Absolute price distance the probability view maps onto 0-100. */
  priceBand: number
  /** Resolved outcomes for the most recent price-against-target markets, oldest to newest. */
  recentMarketResults?: readonly RecentMarketResult[]
  /** Pulses the latest sample while the market is open. */
  live: boolean
  points: readonly ChartPoint[]
  /** Price Feeds symbol; when set with NEXT_PUBLIC_PRICE_FEEDS_URL, the card streams live ticks. */
  priceSymbol?: string
  /** Visible window, usually the market's resolution window. */
  range: TimeRange
  targetLabel: string
  /** Strike the market resolves against; drawn as a centred reference line. */
  targetPrice: number
  timeZone: string
  /** Candidate chart windows, longest last. */
  windows: readonly ChartWindow[]
}

export interface MarketSignal {
  id: string
  /** Where the number comes from, e.g. "Polymarket venue price"; absent for static fixtures. */
  context?: string
  label: string
  side?: MarketSide
  value: string
}

export interface SignalsData {
  browseLabel: string
  items: readonly MarketSignal[]
  title: string
  total: number
}

export interface LegacyOrderBookLevel {
  priceCents: number
  shares: number
  totalUsd: number
}

export type OrderBookLevel =
  | LegacyOrderBookLevel
  | {
      priceTicks: number
      qty: string
      totalMicros: string
      depthPercentage: number
    }

/**
 * The series' Gemini block as last polled. A failed poll keeps the last answer but marks it
 * `poll-failed`, so the book's state reads as unknown rather than as the last thing heard.
 */
export type GeminiBookState =
  | { status: 'polled'; gemini: RustGeminiSeries | undefined }
  | { status: 'poll-failed'; gemini: RustGeminiSeries | undefined }

export interface OrderBookData {
  actionLabel: string
  asks: readonly OrderBookLevel[]
  bids: readonly OrderBookLevel[]
  columnLabels: readonly [string, string, string]
  /** True while the exchange feed is live, so an empty side is known to be empty. */
  feedLive?: boolean
  /** Where the mirrored book comes from; absent without a live exchange. */
  geminiBook?: GeminiBookState
  spreadCents?: number
  spreadTicks?: number | null
  spreadLabel: string
  spreadPercentage?: number
  title: string
}

export interface StrategyPosition {
  /** The position's worth at the current mark. */
  valueCents: Cents
  label: string
  shares: number
  side: MarketSide
}

export interface MyStrategy {
  id: string
  name: string
  position?: StrategyPosition
  status: StrategyStatus
  totalPnlCents: Cents
  trades: number
}

export interface StrategyTab {
  id: StrategyTabId
  label: string
}

export interface MyStrategiesData {
  rows: readonly MyStrategy[]
  tabs: readonly StrategyTab[]
  title: string
}

export interface StrategyCreator {
  /** Absent while the exchange has no portrait for the author; the card shows initials. */
  avatar?: ImageAsset
  handle: string
  username: string
}

export interface PublicStrategy {
  logic: string
  inverseLogic: string
  runners?: readonly ImageAsset[]
  creator: StrategyCreator
  deployedCents: Cents
  id: string
  runningCount: number
  /** The exchange's template id, what Run and Inverse deploy. */
  templateId: string
  title: string
  /**
   * Readable names of the signals either side reads that the exchange cannot act on yet; a
   * deployment made now would sit idle. Absent when every signal is ready.
   */
  waitingOn?: readonly string[]
}

export interface PublicStrategySortOption {
  id: PublicStrategySortId
  label: string
}

export interface StrategyComposerData {
  addContextLabel: string
  dismissTipLabel: string
  inputLabel: string
  placeholder: string
  submitLabel: string
  tipLinkLabel: string
  tipPrefix: string
}

export type ChatPanelView = 'chat' | 'strategies'

export interface StrategyChatSegment {
  /** Renders the segment semibold, for inline emphasis inside an answer. */
  emphasis?: boolean
  text: string
}

export interface StrategyChatChartCardData {
  caption: string
  points: readonly ChartPoint[]
  range: TimeRange
  timeZone: string
  title: string
}

export interface StrategyChatData {
  answerParagraphs: readonly (readonly StrategyChatSegment[])[]
  chartCard: StrategyChatChartCardData
  question: string
}

export interface ChatPanelData {
  availability?: 'ready' | 'unavailable'
  /** A supplied conversation; absent for a new chat. */
  chat?: StrategyChatData
  composer?: StrategyComposerData
  defaultSortId?: PublicStrategySortId
  defaultSortLabel?: string
  publicStrategies: readonly PublicStrategy[]
  sectionTitle?: string
  sortLabel?: string
  sortOptions?: readonly PublicStrategySortOption[]
  subtitle?: string
  title?: string
  view: ChatPanelView
}

export interface MarketDetail {
  liveEvent?: { eventId: number; seriesId: number }
  /** The series' Gemini block at load time; the browser polls it from there. */
  gemini?: RustGeminiSeries
  category: string
  header: MarketHeaderData
  myStrategies?: MyStrategiesData
  orderBook?: OrderBookData
  /** Strike markets show price against the strike as the hero chart instead of probability. */
  priceChart?: MarketPriceChartData
  probabilityChart?: MarketProbabilityChartData
  /** A live game's two teams and tipoff; set only for an NBA round, drives the scoreboard strip. */
  scoreboard?: { home: string; away: string; tipoffMs?: number }
  signals?: SignalsData
  slug: string
  chatPanel?: ChatPanelData
}
