import * as z from 'zod/mini'

import { CategoryPathSchema, EntityIdSchema, IsoDateTimeSchema, SlugSchema } from './primitives'

/*
 * v0 wire contracts for the market catalog, derived from docs/MARKETS-DATA-MODEL.md.
 * Field names mirror the backend verbatim (provenance naming); the app renames only in a
 * `*.mapper.ts`. Two shapes are assumptions until silk-road publishes its API contract and
 * are marked ASSUMPTION below.
 */

/** Seven market types. Everything else is `period`, `stat`, `line`, `subject`. */
export const MarketTypeSchema = z.enum([
  'moneyline',
  'spread',
  'total',
  'outright',
  'updown',
  'threshold',
  'custom',
])

/** The team or player a market is about. `null` means the whole event. */
export const MarketSubjectSchema = z.object({
  id: EntityIdSchema,
  kind: z.enum(['team', 'player']),
  name: z.string(),
})

export const OutcomeSchema = z.object({
  id: EntityIdSchema,
  market_id: EntityIdSchema,
  label: z.string().check(z.minLength(1)),
  sort: z.int().check(z.minimum(0)),
  image: z.nullable(z.url()),
})

export const MarketSchema = z.object({
  id: EntityIdSchema,
  event_id: EntityIdSchema,
  slug: SlugSchema,
  question: z.string().check(z.minLength(1)),
  rules: z.nullable(z.string()),
  type: MarketTypeSchema,
  period: z.nullable(z.string()),
  stat: z.nullable(z.string()),
  // ASSUMPTION: lines such as -5.5 or 221.5 arrive as decimal strings, never floats.
  line: z.nullable(z.string().check(z.regex(/^-?\d+(?:\.\d+)?$/))),
  subject: z.nullable(MarketSubjectSchema),
  is_main_line: z.boolean(),
  image: z.nullable(z.url()),
  settlement_sources: z.array(z.url()),
  outcomes: z.array(OutcomeSchema).check(z.minLength(2)),
})

export const EventSchema = z.object({
  id: EntityIdSchema,
  slug: SlugSchema,
  title: z.string().check(z.minLength(1)),
  category: CategoryPathSchema,
  series_id: z.nullable(EntityIdSchema),
  // The frontend never branches on source; it is kept for display and debugging only.
  source: z.string(),
  image: z.nullable(z.url()),
  opens_at: IsoDateTimeSchema,
  occurs_at: IsoDateTimeSchema,
  closes_at: IsoDateTimeSchema,
  stage: z.nullable(z.string()),
  round: z.nullable(z.string()),
  // ASSUMPTION: markets are embedded on the event read model.
  markets: z.array(MarketSchema),
})

export type MarketType = z.infer<typeof MarketTypeSchema>
export type MarketSubject = z.infer<typeof MarketSubjectSchema>
export type Outcome = z.infer<typeof OutcomeSchema>
export type Market = z.infer<typeof MarketSchema>
export type Event = z.infer<typeof EventSchema>
