import * as z from 'zod/mini'

/**
 * A reference names one live quantity the browser knows how to fetch. A spec is built from
 * references and never carries values, so a widget stays live after the reply finishes
 * and the model never has to move data through the conversation.
 */
const SIGNAL_ID_MAX = 120
const FIELD_NAME_MAX = 64

/** One field of one catalogued signal, the same identity the strip pins by. */
export const SignalRefSchema = z.object({
  type: z.literal('signal'),
  id: z.string().check(z.minLength(1), z.maxLength(SIGNAL_ID_MAX)),
  field: z.string().check(z.minLength(1), z.maxLength(FIELD_NAME_MAX)),
})

/** The BTC/USD price the rounds settle on. BTC series only. */
export const SpotRefSchema = z.object({ type: z.literal('spot') })

/**
 * The market's own price of the quoted side (`up` on BTC): its probability
 * (`up`, whatever the series calls the side), best bid, best ask or last trade.
 */
export const MarketRefSchema = z.object({
  type: z.literal('market'),
  measure: z.enum(['up', 'bid', 'ask', 'last']),
})

/** The BTC price the open round is measured against. BTC series only. */
export const StrikeRefSchema = z.object({ type: z.literal('strike') })

export const RefSchema = z.discriminatedUnion('type', [
  SignalRefSchema,
  SpotRefSchema,
  MarketRefSchema,
  StrikeRefSchema,
])

export type Ref = z.infer<typeof RefSchema>

/** A stable key for a reference, for React keys, legends and de-duplication. */
export function refKey(ref: Ref): string {
  switch (ref.type) {
    case 'signal':
      return `${ref.id}:${ref.field}`
    case 'market':
      return `market:${ref.measure}`
    default:
      return ref.type
  }
}
