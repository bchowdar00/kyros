import 'server-only'

import { createRustExchangeClient, type RustExchangeClient } from '@kyros/api'
import type { EventDetail, RustGeminiSeries, RustSeriesInfo, SeriesRecord } from '@kyros/domain'

import { env } from '@/env'
import {
  seriesIdForIndex,
  seriesIndexForId,
  toEventDetail,
  toSeriesRecord,
} from '@/features/market-detail/exchange-catalog.mapper'

// The catalog now comes from the Rust exchange (`GET /api/series`). The exported names are
// kept so the home, market and active-market loaders stay untouched.

export interface LiveSeriesEvent {
  detail: EventDetail
  /** What the exchange follows on Gemini for this series; absent when it follows nothing. */
  gemini?: RustGeminiSeries
  series: SeriesRecord
}

function client(): RustExchangeClient | undefined {
  const baseUrl = env.EXCHANGE_API_URL

  if (baseUrl === undefined) {
    return undefined
  }

  return createRustExchangeClient({
    baseUrl,
    fetch: (input, init) => fetch(input, { ...init, cache: 'no-store' }),
    retry: { attempts: 1 },
    timeoutMs: 2_000,
  })
}

/** Underlyings the deployment hides (`HIDDEN_UNDERLYINGS`). */
function hiddenUnderlyings(): ReadonlySet<string> {
  return new Set(
    (env.HIDDEN_UNDERLYINGS ?? '')
      .split(',')
      .map(value => value.trim().toLowerCase())
      .filter(value => value.length > 0)
  )
}

/** Whether the deployment shows this series. Hidden series keep their exchange index. */
export function isSeriesVisible(info: Pick<RustSeriesInfo, 'underlying'>): boolean {
  const underlying = info.underlying?.toLowerCase()
  return underlying === undefined || !hiddenUnderlyings().has(underlying)
}

async function listLive(exchange: RustExchangeClient): Promise<LiveSeriesEvent[]> {
  const result = await exchange.listSeries()

  if (!result.ok) {
    return []
  }

  const live: LiveSeriesEvent[] = []

  for (const [index, info] of result.data.entries()) {
    if (!isSeriesVisible(info)) {
      continue
    }

    const series = toSeriesRecord(info, seriesIdForIndex(index))
    const detail = toEventDetail(info, series.id)

    if (detail !== undefined) {
      live.push({ detail, series, ...(info.gemini === undefined ? {} : { gemini: info.gemini }) })
    }
  }

  return live
}

export async function loadLiveSeriesEvents(): Promise<readonly LiveSeriesEvent[]> {
  const exchange = client()

  return exchange === undefined ? [] : listLive(exchange)
}

export async function loadActiveSeriesEvent(
  seriesId: number,
  signal?: AbortSignal
): Promise<EventDetail | undefined> {
  const exchange = client()

  if (exchange === undefined) {
    return undefined
  }

  const result = await exchange.listSeries(signal)
  const info = result.ok ? result.data[seriesIndexForId(seriesId)] : undefined

  return info === undefined || !isSeriesVisible(info) ? undefined : toEventDetail(info, seriesId)
}

export async function loadLiveSeriesEventBySlug(
  slug: string
): Promise<LiveSeriesEvent | undefined> {
  const exchange = client()

  if (exchange === undefined) {
    return undefined
  }

  return (await listLive(exchange)).find(candidate => candidate.series.slug === slug)
}
