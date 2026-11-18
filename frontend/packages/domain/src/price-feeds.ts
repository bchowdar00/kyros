import * as z from 'zod/mini'

// Public Price Feeds REST/SSE contracts; prices stay lossless on the wire.
export const FeedSymbolSchema = z.string().check(z.maxLength(64), z.regex(/^[A-Za-z0-9._/-]+$/))
const mantissa = z.string().check(z.maxLength(20), z.regex(/^-?\d+$/))
const exponent = z.int().check(z.minimum(-2147483648), z.maximum(2147483647))
const timestamp = z.int().check(z.minimum(0), z.maximum(4102444800000000))
export const FeedTickSchema = z.object({
  v: z.literal(1),
  provider: z.string().check(z.maxLength(100)),
  symbol: FeedSymbolSchema,
  mantissa,
  expo: exponent,
  publishTimeUs: timestamp,
})
export const FeedAssetsSchema = z.object({
  assets: z.array(
    z.object({
      symbol: FeedSymbolSchema,
      provider: z.string().check(z.maxLength(100)),
      active: z.boolean(),
      last_tick_at: z.nullable(z.iso.datetime()),
    })
  ),
})
export const FeedHistorySchema = z.object({
  s: z.literal('ok'),
  points: z
    .array(
      z.object({
        t: z.int().check(z.minimum(0), z.maximum(4102444800)),
        mantissa,
        expo: exponent,
      })
    )
    .check(z.maxLength(1000)),
  firstTickAtUs: z.nullable(timestamp),
  lastTickAtUs: z.nullable(timestamp),
})
export type FeedTick = z.infer<typeof FeedTickSchema>
