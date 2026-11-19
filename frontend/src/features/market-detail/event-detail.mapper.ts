import type { EventDetail, SeriesRecord } from '@kyros/domain'

import type { HomeMarketCardModel } from '@/features/home/types'

import { MARKET_DETAIL_FIXTURES } from './market-detail.fixture'
import { resolvePriceMarket } from './active-market.mapper'
import type { ChatPanelData, MarketDetail, PublicStrategy } from './types'

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

export function toMarketDetail(
  series: SeriesRecord,
  detail: EventDetail,
  publicStrategies?: readonly PublicStrategy[]
): MarketDetail | undefined {
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
