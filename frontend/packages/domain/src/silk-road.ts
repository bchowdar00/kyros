import * as z from 'zod/mini'

import { EntityIdSchema, SlugSchema } from './primitives'

export const PriceEvidenceSchema = z.object({
  exponent: z.int(),
  instant: z.string().check(z.minLength(1)),
  mantissa: z.string().check(z.regex(/^-?\d+$/)),
  publishTimeMicros: z.string().check(z.regex(/^-?\d+$/)),
})

export const AutomationEvidenceSchema = z.object({
  kind: z.string().check(z.minLength(1)),
  observation: z.optional(PriceEvidenceSchema),
  priceSymbol: z.string().check(z.minLength(1)),
  reference: PriceEvidenceSchema,
  result: z.optional(z.string()),
  seriesId: EntityIdSchema,
  workflowId: z.string().check(z.minLength(1)),
})

export const MarketMetadataSchema = z.object({
  automation: z.optional(AutomationEvidenceSchema),
})

export const MarketRecordSchema = z.object({
  endsAt: z.string().check(z.minLength(1)),
  eventId: z.optional(z.int()),
  id: EntityIdSchema,
  metadata: MarketMetadataSchema,
  noLabel: z.optional(z.string()),
  question: z.string(),
  rules: z.optional(z.string()),
  slug: SlugSchema,
  startsAt: z.string().check(z.minLength(1)),
  status: z.string(),
  title: z.string(),
  yesLabel: z.optional(z.string()),
})

export const EventDetailSchema = z.object({
  endsAt: z.string().check(z.minLength(1)),
  id: EntityIdSchema,
  image: z.optional(z.string()),
  markets: z.array(MarketRecordSchema),
  seriesId: EntityIdSchema,
  slug: SlugSchema,
  startsAt: z.string().check(z.minLength(1)),
  status: z.string(),
  subtitle: z.optional(z.string()),
  title: z.string(),
})

export const SeriesRecordSchema = z.object({
  assetType: z.optional(z.string()),
  frequency: z.optional(z.string()),
  id: EntityIdSchema,
  image: z.optional(z.string()),
  marketType: z.optional(z.string()),
  slug: SlugSchema,
  status: z.string(),
  subtitle: z.optional(z.string()),
  title: z.optional(z.string()),
})

export const SeriesListPageSchema = z.object({
  hasMore: z.boolean(),
  items: z.array(SeriesRecordSchema),
  nextCursor: z.optional(z.nullable(z.int())),
})

export type PriceEvidence = z.infer<typeof PriceEvidenceSchema>
export type AutomationEvidence = z.infer<typeof AutomationEvidenceSchema>
export type MarketRecord = z.infer<typeof MarketRecordSchema>
export type EventDetail = z.infer<typeof EventDetailSchema>
export type SeriesRecord = z.infer<typeof SeriesRecordSchema>
export type SeriesListPage = z.infer<typeof SeriesListPageSchema>
