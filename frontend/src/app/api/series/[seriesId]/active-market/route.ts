import { EntityIdSchema } from '@kyros/domain'
import * as z from 'zod/mini'

import { toActiveMarketIdentity } from '@/features/market-detail/active-market.mapper'
import { loadActiveSeriesEvent } from '@/server/silk-road.server'

const headers = { 'Cache-Control': 'no-store' }

export async function GET(request: Request, { params }: { params: Promise<{ seriesId: string }> }) {
  const { seriesId } = await params
  const parsedId = z.safeParse(EntityIdSchema, Number(seriesId))

  if (!parsedId.success) {
    return Response.json({ error: 'Invalid series' }, { headers, status: 400 })
  }

  const activeEvent = await loadActiveSeriesEvent(parsedId.data, request.signal)

  const identity = activeEvent === undefined ? undefined : toActiveMarketIdentity(activeEvent)

  if (identity === undefined) {
    return Response.json({ error: 'Active market unavailable' }, { headers, status: 503 })
  }

  return Response.json(identity, { headers })
}
