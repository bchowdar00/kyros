'use client'

import { useQuery } from '@tanstack/react-query'
import { createRustExchangeClient } from '@kyros/api'
import type { RustMarket } from '@kyros/domain'

import { getBookStream, IDLE_BOOK } from '@/features/exchange-streams/book-stream'
import { getPriceStream, IDLE_PRICE_STREAM } from '@/features/exchange-streams/price-stream'
import { getTradesStream, IDLE_TRADES } from '@/features/exchange-streams/trades-stream'
import type { FillLogProps } from '../presets/fill-log'
import type { OrderBookProps } from '../presets/order-book'
import type { RoundResultsProps } from '../presets/round-results'
import { resolveBook, resolveDistribution, resolveFills, resolveRounds } from '../resolve/tables'
import type { BookSpec, DistributionSpec, FillsSpec, RoundsSpec } from '../specs'
import { DISTRIBUTION_ROUNDS_DEFAULT, FILLS_DEFAULT, ROUNDS_DEFAULT } from '../specs/bound'
import type { HistogramProps } from '../widgets/histogram'
import { type Binding, LOADING, unavailable, useStream } from './binding'
import type { ChatData } from './chat-data'

const REQUEST_TIMEOUT_MS = 3_000

export function useBookBinding(spec: BookSpec, data: ChatData): Binding<OrderBookProps> {
  const book = useStream(getBookStream(data.apiUrl, spec.series), IDLE_BOOK)
  if (book.receivedAt === 0) {
    return book.status === 'disconnected'
      ? unavailable('The book socket is disconnected.')
      : LOADING
  }
  return {
    status: book.status === 'disconnected' ? 'stale' : 'live',
    props: resolveBook(spec.series, book, spec.depth),
  }
}

export function useFillsBinding(spec: FillsSpec, data: ChatData): Binding<FillLogProps> {
  const trades = useStream(getTradesStream(data.apiUrl, spec.series), IDLE_TRADES)
  if (spec.scope === 'mine' && data.address === null)
    return unavailable('Sign in to see your fills.')
  if (trades.status === 'connecting') return LOADING
  const scope =
    spec.scope === 'mine' && data.address !== null
      ? ({ kind: 'mine', address: data.address } as const)
      : ({ kind: 'tape' } as const)
  return {
    status: trades.status === 'disconnected' ? 'stale' : 'live',
    props: resolveFills(
      spec.series,
      trades.trades,
      scope,
      spec.limit ?? FILLS_DEFAULT,
      data.timeZone
    ),
  }
}

/**
 * Resolved rounds from the exchange, refetched when the price stream reports a new round
 * so a settlement shows up without polling.
 */
function useRoundsHistory(apiUrl: string, series: string, count: number) {
  const price = useStream(getPriceStream(apiUrl, series), IDLE_PRICE_STREAM)
  const query = useQuery({
    queryKey: ['markets-history', apiUrl, series, count, price.round?.marketId ?? null] as const,
    queryFn: async ({ signal }): Promise<RustMarket[] | null> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).getMarketsHistory(series, count, signal)
      return result.ok ? result.data : null
    },
    retry: false,
    staleTime: Number.POSITIVE_INFINITY,
  })
  return { markets: query.data, price }
}

export function useRoundsBinding(spec: RoundsSpec, data: ChatData): Binding<RoundResultsProps> {
  const count = spec.count ?? ROUNDS_DEFAULT
  const { markets } = useRoundsHistory(data.apiUrl, spec.series, count)
  if (markets === undefined) return LOADING
  if (markets === null) return unavailable('Round history could not be read.')
  const props = resolveRounds(spec.series, markets, count)
  if (props.rounds.length === 0) return unavailable('No round has resolved yet.')
  return { status: 'live', props }
}

export function useDistributionBinding(
  spec: DistributionSpec,
  data: ChatData
): Binding<HistogramProps> {
  const count = spec.count ?? DISTRIBUTION_ROUNDS_DEFAULT
  const { markets, price } = useRoundsHistory(data.apiUrl, spec.series, count)
  if (markets === undefined) return LOADING
  if (markets === null) return unavailable('Round history could not be read.')
  const last = price.points.at(-1)?.value
  const strike = price.round?.strike
  const currentMove = last !== undefined && strike !== undefined ? last - strike : undefined
  const props = resolveDistribution(spec.series, markets, count, currentMove)
  if (props.values.length === 0) return unavailable('No round has resolved yet.')
  return {
    status: 'live',
    props: spec.title === undefined ? props : { ...props, title: spec.title },
  }
}
