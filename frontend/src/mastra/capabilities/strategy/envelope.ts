import * as z from 'zod/mini'
import type { StrategyBrief } from './brief'

/**
 * The bankroll document a build compiles against, derived from the brief's money
 * slots. Three nested budgets, each a share of the one above: the user's allocation,
 * the round budget the strategy is handed each window, and the trade budget one order
 * may spend. A separate stop-after-loss, cumulative across rounds, ends the strategy.
 *
 * Money is in dollars (the exchange's USDC stand-in) as canonical decimal strings;
 * sizes are in contracts. The compiler bakes these values into the program, so a
 * changed envelope means a recompile of the same document.
 */
export const EnvelopeSchema = z.object({
  /** Escrowed sub-account capital: the user's allocation, in dollars. */
  allocation: z.string(),
  /** Hard stop on cumulative realized loss, in dollars. At most the allocation. */
  loss_budget: z.string(),
  /** What one round may commit, in dollars. The strategy sizes off this. */
  round_budget: z.string(),
  /** Per-order cap, in contracts: the trade budget at the worst-case price. */
  max_order_size: z.string(),
  /** Per-round inventory cap, in contracts: the round budget at the worst-case price. */
  max_position_per_market: z.string(),
  /** Resting order count cap. */
  max_open_orders: z.string(),
})

export type Envelope = z.infer<typeof EnvelopeSchema>

/**
 * Dollars one contract can cost at the worst fill. A contract pays $1, and the
 * runtime's risk gate charges a market order at exactly this, so a budget in dollars
 * is the same number of contracts. If the venue's maximum price ever moves, this is
 * the one place the derivation changes.
 */
export const WORST_CASE_PRICE_CENTS = 100

/** Resting orders a taker may hold; a maker re-quotes and gets more. */
const OPEN_ORDERS_TAKER = 4
const OPEN_ORDERS_MAKER = 8

export interface MoneySummary {
  allocation: string
  roundBudget: string
  tradeBudget: string
  stopAfterLoss: string
  /** Contracts one order may hold at the worst-case price. */
  maxOrderContracts: number
}

export type DerivedEnvelope =
  | { ok: true; envelope: Envelope; money: MoneySummary }
  /** A plain sentence Teeny can say back to the user. */
  | { ok: false; problem: string }

type Amount =
  | { kind: 'dollars'; cents: number }
  | { kind: 'fraction'; numerator: number; denominator: number }

const DOLLAR_WORDS = /\b(dollars?|usd|bucks?)\b/i
const NUMBER = /^(\d+)(?:\.(\d+))?$/

/** Parse a plain decimal into an exact numerator over a power of ten. */
function parseDecimal(text: string): { numerator: number; denominator: number } | undefined {
  const match = NUMBER.exec(text)
  if (!match) return undefined
  const whole = match[1] ?? '0'
  const fraction = match[2] ?? ''
  const denominator = 10 ** fraction.length
  const numerator = Number(whole) * denominator + Number(fraction || '0')
  if (!Number.isSafeInteger(numerator)) return undefined
  return { numerator, denominator }
}

/** Dollars as whole cents, rounded down. */
function parseDollars(text: string): number | undefined {
  const cleaned = text.trim().replace(/,/g, '').replace(/^\$/, '').replace(DOLLAR_WORDS, '').trim()
  const parsed = parseDecimal(cleaned)
  if (!parsed) return undefined
  return Math.floor((parsed.numerator * 100) / parsed.denominator)
}

/**
 * What the user said for a share: `10%`, `0.1`, `$10`, `10 dollars`. A bare number
 * at or under one is a fraction; above one it can only be dollars.
 */
export function parseAmount(text: string): Amount | undefined {
  const trimmed = text.trim().replace(/,/g, '')
  if (trimmed.startsWith('$') || DOLLAR_WORDS.test(trimmed)) {
    const cents = parseDollars(trimmed)
    return cents === undefined ? undefined : { kind: 'dollars', cents }
  }
  if (trimmed.endsWith('%')) {
    const parsed = parseDecimal(trimmed.slice(0, -1).trim())
    if (!parsed) return undefined
    return { kind: 'fraction', numerator: parsed.numerator, denominator: parsed.denominator * 100 }
  }
  const parsed = parseDecimal(trimmed)
  if (!parsed) return undefined
  if (parsed.numerator <= parsed.denominator) return { kind: 'fraction', ...parsed }
  return { kind: 'dollars', cents: Math.floor((parsed.numerator * 100) / parsed.denominator) }
}

/** Apply a share to a budget in cents, never exceeding it. */
function applyShare(amount: Amount, baseCents: number): number {
  const cents =
    amount.kind === 'dollars'
      ? amount.cents
      : Math.floor((baseCents * amount.numerator) / amount.denominator)
  return Math.min(cents, baseCents)
}

/** Cents as a canonical decimal string: `250` is `2.5`, `1000` is `10`. */
export function centsToDecimal(cents: number): string {
  const whole = Math.floor(cents / 100)
  const rest = cents % 100
  if (rest === 0) return String(whole)
  return `${whole}.${String(rest).padStart(2, '0').replace(/0$/, '')}`
}

/** Cents as money for people: `250` is `$2.50`, `1000` is `$10`. */
export function centsToMoney(cents: number): string {
  const whole = Math.floor(cents / 100)
  const rest = cents % 100
  return rest === 0 ? `$${whole}` : `$${whole}.${String(rest).padStart(2, '0')}`
}

/** Pure: the brief's money slots to the compiler's envelope, or the reason it cannot be. */
export function deriveEnvelope(brief: StrategyBrief): DerivedEnvelope {
  const allocation = parseDollars(brief.allocation)
  if (allocation === undefined || allocation <= 0)
    return { ok: false, problem: 'The allocation must be a dollar amount above zero.' }

  const roundShare = parseAmount(brief.round_share)
  if (!roundShare)
    return { ok: false, problem: 'The round share must be a fraction like 10% or a dollar amount.' }
  const roundBudget = applyShare(roundShare, allocation)
  if (roundBudget < WORST_CASE_PRICE_CENTS)
    return {
      ok: false,
      problem: `Each round would see ${centsToMoney(roundBudget)}, less than one contract; raise the allocation or the round share.`,
    }

  const tradeShare = parseAmount(brief.trade_share)
  if (!tradeShare)
    return { ok: false, problem: 'The trade share must be a fraction like 25% or a dollar amount.' }
  const tradeBudget = applyShare(tradeShare, roundBudget)
  if (tradeBudget < WORST_CASE_PRICE_CENTS)
    return {
      ok: false,
      problem: `Each order could spend ${centsToMoney(tradeBudget)}, less than one contract; raise the trade share or the round budget.`,
    }

  const stop = parseDollars(brief.stop_after_loss)
  if (stop === undefined || stop <= 0)
    return { ok: false, problem: 'The stop-after-loss must be a dollar amount above zero.' }
  const stopAfterLoss = Math.min(stop, allocation)

  const maxOrderContracts = Math.floor(tradeBudget / WORST_CASE_PRICE_CENTS)
  const maxPositionContracts = Math.floor(roundBudget / WORST_CASE_PRICE_CENTS)
  const openOrders = brief.entry_style === 'limit' ? OPEN_ORDERS_MAKER : OPEN_ORDERS_TAKER

  return {
    ok: true,
    envelope: {
      allocation: centsToDecimal(allocation),
      loss_budget: centsToDecimal(stopAfterLoss),
      round_budget: centsToDecimal(roundBudget),
      max_order_size: String(maxOrderContracts),
      max_position_per_market: String(maxPositionContracts),
      max_open_orders: String(openOrders),
    },
    money: {
      allocation: centsToMoney(allocation),
      roundBudget: centsToMoney(roundBudget),
      tradeBudget: centsToMoney(tradeBudget),
      stopAfterLoss: centsToMoney(stopAfterLoss),
      maxOrderContracts,
    },
  }
}

/**
 * The one line the proposal card and the readback say: what goes in, what a round
 * sees, what an order may spend, and when it stops. Falls back to the problem.
 */
export function describeMoney(brief: StrategyBrief): string {
  const derived = deriveEnvelope(brief)
  if (!derived.ok) return derived.problem
  const { allocation, roundBudget, tradeBudget, stopAfterLoss } = derived.money
  return `${allocation} in, sees ${roundBudget} per round, up to ${tradeBudget} per order, stops after losing ${stopAfterLoss}`
}
