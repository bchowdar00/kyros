import { RustSeriesSlugSchema } from '@tiny/domain'
import * as z from 'zod/mini'

import { TITLE_MAX } from './bound'

/**
 * Widgets that send one event back into the conversation and then go quiet. Their
 * derivations are named, never functions, because a spec travels as JSON.
 */
export const PICKER_OPTIONS_MIN = 2
export const PICKER_OPTIONS_MAX = 6
/** A stake beyond this is a mistake, not a trade, at the demo's account size. */
export const TICKET_AMOUNT_CENTS_MAX = 100_000
const SHORT_MAX = 200

const title = z.optional(z.string().check(z.minLength(1), z.maxLength(TITLE_MAX)))
const short = z.string().check(z.minLength(1), z.maxLength(SHORT_MAX))

/** A trade to confirm; the prices bind live and the submit sends a confirmation. */
export const TicketSpecSchema = z.object({
  kind: z.literal('ticket'),
  series: RustSeriesSlugSchema,
  title,
  side: z.enum(['YES', 'NO']),
  amount_cents: z.optional(z.int().check(z.minimum(100), z.maximum(TICKET_AMOUNT_CENTS_MAX))),
})

export const PickerSpecSchema = z.object({
  kind: z.literal('picker'),
  title,
  prompt: short,
  options: z
    .array(
      z.object({
        id: z.string().check(z.minLength(1), z.maxLength(32)),
        title: short,
        description: z.optional(short),
        meta: z.optional(z.string().check(z.maxLength(40))),
      })
    )
    .check(z.minLength(PICKER_OPTIONS_MIN), z.maxLength(PICKER_OPTIONS_MAX)),
  default_option_id: z.optional(z.string().check(z.maxLength(32))),
  confirm_label: z.optional(z.string().check(z.minLength(1), z.maxLength(40))),
})

/**
 * One variable to drag with readouts that follow it. `btc_move` derives the fair
 * probability, the mark and the settlement P&L of a stake; `stake` derives the payout
 * and the loss at the current price.
 */
export const ScenarioSpecSchema = z.object({
  kind: z.literal('scenario'),
  series: RustSeriesSlugSchema,
  title,
  variable: z.enum(['btc_move', 'stake']),
  side: z.optional(z.enum(['YES', 'NO'])),
  stake_cents: z.optional(z.int().check(z.minimum(100), z.maximum(TICKET_AMOUNT_CENTS_MAX))),
})

export type TicketSpec = z.infer<typeof TicketSpecSchema>
export type PickerSpec = z.infer<typeof PickerSpecSchema>
export type ScenarioSpec = z.infer<typeof ScenarioSpecSchema>
