import * as z from 'zod/mini'
import { described } from './describe'
import { type Sides, sidesOf } from './sides'

/**
 * The strategy brief: what Teeny agrees with the user before anything is built. It is
 * written in the user's terms; the writer turns it into a document. Every slot is
 * required so the model states its assumption instead of leaving a hole.
 */
export const StrategyBriefSchema = z.object({
  series: described(
    z.string(),
    'The market series slug this chat is open on, e.g. `btc-2m`. Filled in from the page; never asked.'
  ),
  summary: described(
    z.string(),
    'One plain sentence saying how the strategy works, for the card the user confirms on: what it watches, what it does, when it stops. No numbers about money.'
  ),
  goal: described(z.string(), 'The user’s intent in one or two sentences, in their words.'),
  situations: described(
    z.optional(z.array(z.string())),
    'The situations the strategy can be in, in the user’s words, one per mode: e.g. `hunting`, `holding up`, `holding down`. A plain entry with an exit is two; a flip is three.'
  ),
  entry: described(
    z.string(),
    'When to enter, in plain language, naming the signals, fields and levels involved.'
  ),
  signal_ids: described(
    z.array(z.string()),
    'Registry signal ids the entry or exit reads, from listMarketSignals. Empty when only the book is used.'
  ),
  outcome: described(
    z.string(),
    'Which side to buy, by the market’s own label: `up` or `down` on a BTC series, `home` or `away` on an NBA series; `either` when the rule picks the side at fire time.'
  ),
  sides: described(
    z.optional(z.array(z.string())),
    'The market’s two side labels, quoted side first. Filled in from the page; leave it out.'
  ),
  recurrence: described(
    z.enum(['once_per_round', 'every_time', 'on_interval']),
    'Fire once per round, every time the condition becomes true, or on a fixed interval.'
  ),
  interval: described(
    z.nullable(z.string()),
    'The period for `on_interval`, e.g. `5s`; else null.'
  ),
  entry_style: described(
    z.enum(['market', 'limit']),
    '`market` takes the price on the book; `limit` rests an order and waits.'
  ),
  exit: described(
    z.string(),
    'When to get out: a take-profit, a stop, a time before close, or `hold to settlement`.'
  ),
  allocation: described(
    z.string(),
    'Dollars the strategy is first sized against, as a plain decimal. `100` unless the user volunteered an amount; the deploy screen is where they set it, so never ask.'
  ),
  round_share: described(
    z.string(),
    'What the strategy is handed each round, as the user said it: a fraction of the allocation (`0.1` or `10%`) or a dollar amount (`$10`).'
  ),
  trade_share: described(
    z.string(),
    'What one order may spend, as the user said it: a fraction of the round budget (`0.25` or `25%`) or a dollar amount (`$2.50`).'
  ),
  stop_after_loss: described(
    z.string(),
    'Dollars of cumulative loss across rounds at which the strategy stops for good, as a plain decimal. The whole allocation unless the user says otherwise.'
  ),
  stop_before_close: described(
    z.string(),
    'How long before the round closes to stop placing new orders, e.g. `10s`.'
  ),
  assumptions: described(
    z.array(z.string()),
    'Every choice made on the user’s behalf that they did not state, one per entry.'
  ),
})

export type StrategyBrief = z.infer<typeof StrategyBriefSchema>

/** The sides of the brief's market, quoted side first; BTC's when the page did not say. */
export function briefSides(brief: Pick<StrategyBrief, 'sides'>): Sides {
  return sidesOf(brief.sides)
}

export function renderBrief(brief: StrategyBrief) {
  const sides = briefSides(brief)
  const lines = [
    `Market: ${brief.series} (sides: ${sides.quoted}, the quoted side, and ${sides.other})`,
    `Summary: ${brief.summary}`,
    `Goal: ${brief.goal}`,
    `Situations: ${brief.situations?.length ? brief.situations.join('; ') : 'not stated'}`,
    `Entry: ${brief.entry}`,
    `Signals: ${brief.signal_ids.length ? brief.signal_ids.join(', ') : 'the market book only'}`,
    `Side: ${brief.outcome}`,
    `Recurrence: ${brief.recurrence}${brief.interval ? ` every ${brief.interval}` : ''}`,
    `Entry style: ${brief.entry_style}`,
    `Exit: ${brief.exit}`,
    `Money: $${brief.allocation} allocated; each round sees ${brief.round_share} of it; one order may spend ${brief.trade_share} of the round; stops for good after losing $${brief.stop_after_loss}`,
    `Stop placing orders before close: ${brief.stop_before_close}`,
  ]
  if (brief.assumptions.length)
    lines.push(`Assumptions: ${brief.assumptions.map(a => `\n- ${a}`).join('')}`)
  return lines.join('\n')
}
