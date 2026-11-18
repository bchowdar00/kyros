import {
  MarketRecordSchema,
  SeriesListPageSchema,
  type EventDetail,
  type SeriesListPage,
} from '@tiny/domain'
import * as z from 'zod/mini'

import { type ApiClient, type ApiClientOptions, createApiClient } from './client'
import { ApiError, type ApiResult } from './errors'

export interface SilkRoadClient {
  getActiveEvent(seriesId: number, signal?: AbortSignal): Promise<ApiResult<EventDetail>>
  getEvent(eventId: number, signal?: AbortSignal): Promise<ApiResult<EventDetail>>
  listSeries(
    searchParams?: { cursor?: number; limit?: number; status?: string },
    signal?: AbortSignal
  ): Promise<ApiResult<SeriesListPage>>
}

const EventSummarySchema = z.object({
  endsAt: z.string(),
  id: z.int(),
  image: z.optional(z.string()),
  seriesId: z.int(),
  slug: z.string(),
  startsAt: z.string(),
  status: z.string(),
  subtitle: z.optional(z.string()),
  title: z.string(),
})

const EventListPageSchema = z.object({
  hasMore: z.boolean(),
  items: z.array(EventSummarySchema),
  nextCursor: z.optional(z.nullable(z.int())),
})

const MarketListPageSchema = z.object({
  hasMore: z.boolean(),
  items: z.array(MarketRecordSchema),
  nextCursor: z.optional(z.nullable(z.int())),
})

type EventSummary = z.infer<typeof EventSummarySchema>

const PAGE_SIZE = 100

function missing(message: string): ApiResult<never> {
  return {
    ok: false,
    error: new ApiError('http', message, { retryable: false, status: 404 }),
  }
}

function invalidCursor(): ApiResult<never> {
  return {
    ok: false,
    error: new ApiError('parse', 'Silk Road returned a non-advancing cursor', {
      retryable: false,
    }),
  }
}

function pickActiveEvent(events: readonly EventSummary[]): EventSummary | undefined {
  const now = Date.now()
  const active = events
    .filter(event => event.status === 'EVENT_STATUS_ACTIVE')
    .toSorted((left, right) => Date.parse(right.startsAt) - Date.parse(left.startsAt))

  return (
    active.find(event => {
      const startsAt = Date.parse(event.startsAt)
      const endsAt = Date.parse(event.endsAt)
      return Number.isFinite(startsAt) && Number.isFinite(endsAt) && startsAt <= now && now < endsAt
    }) ?? active[0]
  )
}

export function createSilkRoadClient(options: ApiClientOptions): SilkRoadClient {
  const client: ApiClient = createApiClient(options)

  async function listEvents(
    searchParams: { seriesId?: number },
    signal?: AbortSignal
  ): Promise<ApiResult<EventSummary[]>> {
    const events: EventSummary[] = []
    let cursor: number | undefined

    for (;;) {
      const response = await client.get('/v1/events', {
        schema: EventListPageSchema,
        searchParams: { ...searchParams, cursor, limit: PAGE_SIZE },
        ...(signal === undefined ? {} : { signal }),
      })

      if (!response.ok) {
        return response
      }

      events.push(...response.data.items)
      if (!response.data.hasMore) {
        return { ok: true, data: events }
      }

      const nextCursor = response.data.nextCursor
      if (nextCursor === undefined || nextCursor === null || nextCursor <= (cursor ?? 0)) {
        return invalidCursor()
      }
      cursor = nextCursor
    }
  }

  async function eventDetail(
    event: EventSummary,
    signal?: AbortSignal
  ): Promise<ApiResult<EventDetail>> {
    const markets = await client.get('/v1/markets', {
      schema: MarketListPageSchema,
      searchParams: { eventId: event.id, limit: PAGE_SIZE },
      ...(signal === undefined ? {} : { signal }),
    })

    return markets.ok ? { ok: true, data: { ...event, markets: markets.data.items } } : markets
  }

  return {
    async getActiveEvent(seriesId, signal) {
      const events = await listEvents({ seriesId }, signal)
      if (!events.ok) {
        return events
      }

      const event = pickActiveEvent(events.data)
      return event === undefined
        ? missing('no active event for this series')
        : eventDetail(event, signal)
    },
    async getEvent(eventId, signal) {
      const events = await listEvents({}, signal)
      if (!events.ok) {
        return events
      }

      const event = events.data.find(candidate => candidate.id === eventId)
      return event === undefined ? missing('no such event') : eventDetail(event, signal)
    },
    listSeries(searchParams, signal) {
      return client.get('/v1/series', {
        schema: SeriesListPageSchema,
        ...(searchParams === undefined ? {} : { searchParams }),
        ...(signal === undefined ? {} : { signal }),
      })
    },
  }
}
