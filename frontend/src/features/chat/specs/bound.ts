import { RustSeriesSlugSchema } from '@kyros/domain'
import * as z from 'zod/mini'

import { RefSchema } from './refs'

/**
 * Widgets bound to the exchange and signal stores. Each names a series and what to read;
 * the browser fetches, formats and keeps it live. Bounds live here so a tool and the
 * renderer refuse the same things.
 */
export const TITLE_MAX = 80
/** The bus retains twenty minutes; a chart window never asks for more. */
export const WINDOW_MS_MAX = 20 * 60 * 1000
export const WINDOW_MS_MIN = 30 * 1000
export const WINDOW_MS_DEFAULT = 4 * 60 * 1000
/** The YES tick grid has 153 prices; a ladder can show all of them. */
export const BOOK_DEPTH_MAX = 153
export const BOOK_DEPTH_DEFAULT = 7
export const CHART_REFS_MAX = 4
export const ROUNDS_MAX = 50
export const ROUNDS_DEFAULT = 12
export const DISTRIBUTION_ROUNDS_MIN = 5
export const DISTRIBUTION_ROUNDS_DEFAULT = 30
export const FILLS_MAX = 50
export const FILLS_DEFAULT = 8
export const CANDLE_BUCKET_MS_MIN = 5_000
export const CANDLE_BUCKET_MS_MAX = 5 * 60 * 1000
export const CANDLE_BUCKET_MS_DEFAULT = 10_000

const title = z.optional(z.string().check(z.minLength(1), z.maxLength(TITLE_MAX)))
const windowMs = z.optional(z.int().check(z.minimum(WINDOW_MS_MIN), z.maximum(WINDOW_MS_MAX)))

/** One or more references on a time axis. */
export const ChartSpecSchema = z.object({
  kind: z.literal('chart'),
  series: RustSeriesSlugSchema,
  title,
  refs: z.array(RefSchema).check(z.minLength(1), z.maxLength(CHART_REFS_MAX)),
  window_ms: windowMs,
  /** Draw the strike as a guide and shade the winning side; only meaningful with `spot`. */
  show_strike: z.optional(z.boolean()),
  /** Mark the round's open on the time axis. */
  mark_open: z.optional(z.boolean()),
})

/** BTC spot as candles, with the strike as a guide. */
export const CandlesSpecSchema = z.object({
  kind: z.literal('candles'),
  series: RustSeriesSlugSchema,
  title,
  window_ms: windowMs,
  bucket_ms: z.optional(
    z.int().check(z.minimum(CANDLE_BUCKET_MS_MIN), z.maximum(CANDLE_BUCKET_MS_MAX))
  ),
})

/** The canonical YES book as a ladder. */
export const BookSpecSchema = z.object({
  kind: z.literal('book'),
  series: RustSeriesSlugSchema,
  title,
  depth: z.optional(z.int().check(z.minimum(1), z.maximum(BOOK_DEPTH_MAX))),
})

/** The whole round at a glance: spot, strike, YES, spread, clock. */
export const MarketSpecSchema = z.object({
  kind: z.literal('market'),
  series: RustSeriesSlugSchema,
  title,
})

/** The round's activity: volume, open interest, trades, traders, largest trade. */
export const StatsSpecSchema = z.object({
  kind: z.literal('stats'),
  series: RustSeriesSlugSchema,
  title,
})

/** The user's position in the series. */
export const PositionSpecSchema = z.object({
  kind: z.literal('position'),
  series: RustSeriesSlugSchema,
  title,
})

/** Recent round outcomes as a strip of bars. */
export const RoundsSpecSchema = z.object({
  kind: z.literal('rounds'),
  series: RustSeriesSlugSchema,
  title,
  count: z.optional(z.int().check(z.minimum(1), z.maximum(ROUNDS_MAX))),
})

/** The distribution of recent rounds' moves from strike, with the open round pinned. */
export const DistributionSpecSchema = z.object({
  kind: z.literal('distribution'),
  series: RustSeriesSlugSchema,
  title,
  count: z.optional(z.int().check(z.minimum(DISTRIBUTION_ROUNDS_MIN), z.maximum(ROUNDS_MAX))),
})

/** The user's fills, or the whole tape. */
export const FillsSpecSchema = z.object({
  kind: z.literal('fills'),
  series: RustSeriesSlugSchema,
  title,
  scope: z.enum(['mine', 'tape']),
  limit: z.optional(z.int().check(z.minimum(1), z.maximum(FILLS_MAX))),
})

export type ChartSpec = z.infer<typeof ChartSpecSchema>
export type CandlesSpec = z.infer<typeof CandlesSpecSchema>
export type BookSpec = z.infer<typeof BookSpecSchema>
export type MarketSpec = z.infer<typeof MarketSpecSchema>
export type StatsSpec = z.infer<typeof StatsSpecSchema>
export type PositionSpec = z.infer<typeof PositionSpecSchema>
export type RoundsSpec = z.infer<typeof RoundsSpecSchema>
export type DistributionSpec = z.infer<typeof DistributionSpecSchema>
export type FillsSpec = z.infer<typeof FillsSpecSchema>
