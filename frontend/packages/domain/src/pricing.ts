import * as z from 'zod/mini'

import { CentsSchema, EntityIdSchema, IsoDateTimeSchema, ProbabilityBpsSchema } from './primitives'

/**
 * A live quote for one outcome. Price and probability are the same fact in two units:
 * a share pays $1.00, so 31¢ is 3,100 bps. Both are sent so the client never derives one
 * from the other with arithmetic of its own.
 */
export const OutcomeQuoteSchema = z.object({
  outcome_id: EntityIdSchema,
  price_cents: CentsSchema,
  probability_bps: ProbabilityBpsSchema,
  as_of: IsoDateTimeSchema,
})

export type OutcomeQuote = z.infer<typeof OutcomeQuoteSchema>
