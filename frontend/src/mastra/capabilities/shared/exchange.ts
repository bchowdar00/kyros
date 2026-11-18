import { createRustExchangeClient, type RustExchangeClient } from '@tiny/api'
import { RustSeriesSlugSchema } from '@tiny/domain'
import * as z from 'zod/mini'

import type { CapabilityEnv } from '../types'
import { readChatContext, STANDALONE_CHAT } from '../../request-context'

/** The series every tool falls back to when neither the input nor the page names one. */
export const DEFAULT_SERIES = 'btc-2m'
const REQUEST_TIMEOUT_MS = 5_000
const TENTHS_PER_CENT = 10

/** The Rust exchange origin the server reads from, preferring the server-only variable. */
export function exchangeUrl(env: CapabilityEnv): string | undefined {
  return env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL
}

export function exchangeClient(baseUrl: string): RustExchangeClient {
  return createRustExchangeClient({
    baseUrl,
    timeoutMs: REQUEST_TIMEOUT_MS,
    retry: { attempts: 1 },
    fetch: (input, init) => fetch(input, { ...init, cache: 'no-store', redirect: 'error' }),
  })
}

/** An optional series in a tool's input; absent means the page's market, then the default. */
export const SeriesInputSchema = z.optional(RustSeriesSlugSchema)

/**
 * The series a tool works on: the input when given, else the market the chat was opened
 * from when it is a series slug, else the default.
 */
export function resolveSeries(
  input: string | undefined,
  context: { requestContext?: { getRaw(key: string): unknown } | undefined } | undefined
): string {
  if (input !== undefined) return input
  const { marketSlug } = readChatContext(context)
  return marketSlug !== undefined &&
    marketSlug !== STANDALONE_CHAT &&
    RustSeriesSlugSchema.safeParse(marketSlug).success
    ? marketSlug
    : DEFAULT_SERIES
}

export const UNAVAILABLE = { status: 'unavailable', reason: 'The exchange is not configured.' }

export function failed(reason: string, series?: string) {
  return { status: 'unavailable', ...(series === undefined ? {} : { series }), reason }
}

/** Tenths of a cent, as the exchange counts money, to whole cents. */
export function tenthsToCents(tenths: number): number {
  return Math.round(tenths / TENTHS_PER_CENT)
}
