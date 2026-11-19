import type { EventDetail, RustSeriesInfo, SeriesRecord } from '@kyros/domain'

/**
 * The Rust exchange numbers series 0, 1, … and rounds 1, 2, …; the app's `EntityId`
 * contract wants six-digit ids, so both are offset into that range and back.
 */
const SERIES_ID_BASE = 100_001
const EVENT_ID_BASE = 100_000

export function seriesIdForIndex(index: number): number {
  return SERIES_ID_BASE + index
}

export function seriesIndexForId(seriesId: number): number {
  return seriesId - SERIES_ID_BASE
}

export function eventIdForMarket(marketId: number): number {
  return EVENT_ID_BASE + marketId
}

export function marketIdForEvent(eventId: number): number {
  return eventId - EVENT_ID_BASE
}

const SECONDS_PER_MINUTE = 60

function frequencyLabel(durationSecs: number): string {
  return durationSecs % SECONDS_PER_MINUTE === 0
    ? `${durationSecs / SECONDS_PER_MINUTE}m`
    : `${durationSecs}s`
}

export function toSeriesRecord(info: RustSeriesInfo, seriesId: number): SeriesRecord {
  const frequency = frequencyLabel(info.duration_secs)

  return {
    assetType: 'btc',
    frequency,
    id: seriesId,
    marketType: 'updown',
    slug: info.series,
    status: 'SERIES_STATUS_ACTIVE',
    title: `BTC Up or Down ${frequency}`,
  }
}

/**
 * One event per open round. The strike is not on `GET /api/series`; a zero reference tells
 * the mapper it is unknown and the browser's price stream supplies it from its snapshot.
 */
export function toEventDetail(info: RustSeriesInfo, seriesId: number): EventDetail | undefined {
  if (info.current_market_id === null || info.opens_at_ms === null || info.closes_at_ms === null) {
    return undefined
  }

  const eventId = eventIdForMarket(info.current_market_id)
  const startsAt = new Date(info.opens_at_ms).toISOString()
  const endsAt = new Date(info.closes_at_ms).toISOString()
  const slug = `${info.series}-${info.current_market_id}`

  return {
    endsAt,
    id: eventId,
    markets: [
      {
        endsAt,
        eventId,
        id: eventId,
        metadata: {
          automation: {
            kind: 'btc_updown',
            priceSymbol: info.series,
            reference: {
              exponent: 0,
              instant: startsAt,
              mantissa: '0',
              publishTimeMicros: String(info.opens_at_ms * 1_000),
            },
            seriesId,
            workflowId: `exchange:${slug}`,
          },
        },
        noLabel: 'DOWN',
        question: `Will BTC be above its ${startsAt} price at ${endsAt}?`,
        slug: `${slug}-btc-updown`,
        startsAt,
        status: 'MARKET_STATUS_ACTIVE',
        title: 'BTC price: UP or DOWN',
        yesLabel: 'UP',
      },
    ],
    seriesId,
    slug,
    startsAt,
    status: 'EVENT_STATUS_ACTIVE',
    title: `BTC price direction: ${startsAt} to ${endsAt}`,
  }
}
