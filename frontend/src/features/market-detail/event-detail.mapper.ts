import type { EventDetail, MarketRecord, RustGeminiSeries, SeriesRecord } from '@kyros/domain'

import type { HomeMarketCardModel } from '@/features/home/types'

import { MARKET_DETAIL_FIXTURES } from './market-detail.fixture'
import { resolveActiveMarket, resolvePriceMarket, windowRange } from './active-market.mapper'
import type {
  ChatPanelData,
  MarketDetail,
  MarketOutcome,
  ProbabilitySeries,
  PublicStrategy,
} from './types'

const HEADER_SURFACE = '/assets/market/header-surface.png'
const prototypeChrome = MARKET_DETAIL_FIXTURES['btc-above-118k-0903']

function seriesDisplayName(series: SeriesRecord): string {
  if (series.title !== undefined && series.title.length > 0) {
    return series.title
  }

  const name = [
    series.assetType === 'btc' ? 'BTC' : undefined,
    series.marketType === 'updown' ? 'Up or Down' : undefined,
    series.frequency,
  ]
    .filter((part): part is string => part !== undefined && part.length > 0)
    .join(' ')

  return name.length > 0 ? name : series.slug
}

/**
 * The strategies column. Cards come from the exchange's template catalogue; without one
 * the column fails closed rather than showing anything invented.
 */
export function toChatPanel(
  publicStrategies: readonly PublicStrategy[] | undefined
): ChatPanelData {
  return publicStrategies === undefined
    ? { publicStrategies: [], availability: 'unavailable', view: 'strategies' }
    : { publicStrategies, availability: 'ready', view: 'strategies' }
}

/** An NBA round's matchup, read from the market's outcome labels (`yes` = home team). */
function nbaMatchup(market: MarketRecord): { away: string; home: string } | undefined {
  return market.metadata.automation?.kind === 'nba_h2h' &&
    market.yesLabel !== undefined &&
    market.noLabel !== undefined
    ? { away: market.noLabel, home: market.yesLabel }
    : undefined
}

function nbaOutcomes(home: string, away: string): readonly [MarketOutcome, MarketOutcome] {
  return [
    { label: home, side: 'home' },
    { label: away, side: 'away' },
  ]
}

function nbaProbabilitySeries(
  home: string,
  away: string
): readonly [ProbabilitySeries, ProbabilitySeries] {
  return [
    { label: home, points: [], side: 'home' },
    { label: away, points: [], side: 'away' },
  ]
}

function toNbaMarketDetail(
  series: SeriesRecord,
  detail: EventDetail,
  market: MarketRecord,
  matchup: { away: string; home: string },
  gemini: RustGeminiSeries | undefined,
  publicStrategies: readonly PublicStrategy[] | undefined
): MarketDetail | undefined {
  const range = windowRange(detail)

  if (range === undefined) {
    return undefined
  }

  const { away, home } = matchup

  return {
    category: 'Sports',
    liveEvent: { eventId: detail.id, seriesId: series.id },
    header: {
      outcomes: nbaOutcomes(home, away),
      surfaceImage: { alt: '', src: HEADER_SURFACE },
      subtitle: market.question,
      title: market.title,
    },
    myStrategies: prototypeChrome.myStrategies,
    orderBook: {
      title: 'Order book',
      actionLabel: 'Center order book',
      columnLabels: [`Price (${home})`, 'Lots', 'Total'],
      asks: [],
      bids: [],
      spreadLabel: 'Spread',
      spreadTicks: null,
    },
    probabilityChart: {
      live: true,
      range,
      series: nbaProbabilitySeries(home, away),
      seriesSlug: series.slug,
      timeZone: 'UTC',
    },
    scoreboard: {
      home,
      away,
      ...(gemini?.starts_at_ms === undefined ? {} : { tipoffMs: gemini.starts_at_ms }),
    },
    signals: prototypeChrome.signals,
    slug: series.slug,
    chatPanel: toChatPanel(publicStrategies),
  }
}

export function toMarketDetail(
  series: SeriesRecord,
  detail: EventDetail,
  publicStrategies?: readonly PublicStrategy[],
  gemini?: RustGeminiSeries
): MarketDetail | undefined {
  const active = resolveActiveMarket(detail)

  if (active === undefined) {
    return undefined
  }

  const matchup = nbaMatchup(active)

  if (matchup !== undefined) {
    return toNbaMarketDetail(series, detail, active, matchup, gemini, publicStrategies)
  }

  const resolved = resolvePriceMarket(detail)

  if (resolved === undefined) {
    return undefined
  }

  const { market, priceChart } = resolved

  return {
    category: 'Crypto',
    liveEvent: { eventId: detail.id, seriesId: series.id },
    header: {
      // YES first: the header renders the tuple in order, and "Buy Up" leads.
      outcomes: [
        { label: market.yesLabel ?? 'UP', side: 'home' },
        { label: market.noLabel ?? 'DOWN', side: 'away' },
      ],
      surfaceImage: { alt: '', src: HEADER_SURFACE },
      subtitle: market.question,
      title: seriesDisplayName(series),
    },
    myStrategies: prototypeChrome.myStrategies,
    orderBook: {
      title: 'Order book',
      actionLabel: 'Center order book',
      columnLabels: ['Price (YES)', 'Lots', 'Total'],
      asks: [],
      bids: [],
      spreadLabel: 'Spread',
      spreadTicks: null,
    },
    priceChart,
    signals: prototypeChrome.signals,
    slug: series.slug,
    chatPanel: toChatPanel(publicStrategies),
  }
}

export function toHomeMarketCard(
  series: SeriesRecord,
  detail: EventDetail
): HomeMarketCardModel | undefined {
  const active = resolveActiveMarket(detail)
  const matchup = active === undefined ? undefined : nbaMatchup(active)

  if (active !== undefined && matchup !== undefined) {
    const range = windowRange(detail)

    if (range === undefined) {
      return undefined
    }

    const { away, home } = matchup

    return {
      category: 'Sports',
      live: true,
      outcomes: nbaOutcomes(home, away),
      range,
      series: nbaProbabilitySeries(home, away),
      slug: series.slug,
      subtitle: active.question,
      timeZone: 'UTC',
      title: active.title,
    }
  }

  const resolved = resolvePriceMarket(detail)

  if (resolved === undefined) {
    return undefined
  }

  const { market, priceChart } = resolved

  return {
    category: 'Crypto',
    live: true,
    outcomes: [
      { label: market.yesLabel ?? 'UP', side: 'home' },
      { label: market.noLabel ?? 'DOWN', side: 'away' },
    ],
    priceChart: {
      label: priceChart.label,
      points: priceChart.points,
      targetLabel: priceChart.targetLabel,
      targetPrice: priceChart.targetPrice,
    },
    range: priceChart.range,
    slug: series.slug,
    subtitle: market.question,
    timeZone: 'UTC',
    title: seriesDisplayName(series),
  }
}
