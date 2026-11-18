import type { RustMarket, RustMarketStats } from '@tiny/domain'
import { cents, createMoneyFormatter } from '@tiny/money'
import type { RecentMarketResult } from '@tiny/ui/charts/recent-market-results'

import type { SignalsData } from './types'

const money = createMoneyFormatter('en-US')
const TENTHS_PER_CENT = 10

/** Resolved rounds arrive newest first; the strip reads oldest to newest. */
export function toRecentMarketResults(history: readonly RustMarket[]): RecentMarketResult[] {
  const results: RecentMarketResult[] = []

  for (const market of history) {
    if (market.status.kind === 'resolved') {
      results.push(market.status.outcome === 'yes' ? 'above' : 'below')
    }
  }

  return results.toReversed()
}

function usd(tenths: number): string {
  return money.usd(cents(Math.round(tenths / TENTHS_PER_CENT)))
}

/**
 * The strip the page renders before the live catalogue arrives: the two stats cards of the
 * default pins (`defaultPins`). The spot prices have no server-side source, so they join
 * once the catalogue loads.
 */
export function toStatsSignals(stats: RustMarketStats): SignalsData {
  const items: SignalsData['items'] = [
    { id: 'market-volume', label: 'Round Volume', value: usd(stats.total_volume_tenths) },
    { id: 'open-interest', label: 'Open Interest', value: usd(stats.total_locked_tenths) },
  ]

  return { browseLabel: 'Browse All', items, title: 'Signals', total: items.length }
}
