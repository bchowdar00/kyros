import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { deriveEnvelope } from '@/mastra/capabilities/strategy/envelope'

/**
 * The bankroll the deploy screen edits, in the brief's own money slots. Teeny fills the
 * slots when it writes the brief; the user changes dollar figures here. A slot the user has
 * not touched keeps Teeny's share, so the round and order budgets follow the allocation as
 * it is retyped; a touched slot is a dollar amount and stays put.
 */
export interface Bankroll {
  /** Dollars, as typed. */
  allocation: string
  /** Dollars per round, as typed; `null` keeps the brief's share of the allocation. */
  roundBudget: string | null
  /** Dollars per order, as typed; `null` keeps the brief's share of the round. */
  tradeBudget: string | null
  /** Dollars of cumulative loss that end the strategy, as typed. */
  stopAfterLoss: string
}

/** The allocation input keeps two decimals; the brief stores a plain decimal. */
export function toInputAmount(amount: string) {
  const [whole = '0', fraction = ''] = amount.replace(/^\$/, '').split('.')
  return `${whole}.${fraction.padEnd(2, '0').slice(0, 2)}`
}

export function initialBankroll(brief: StrategyBrief): Bankroll {
  return {
    allocation: toInputAmount(brief.allocation),
    roundBudget: null,
    tradeBudget: null,
    stopAfterLoss: toInputAmount(brief.stop_after_loss),
  }
}

/** The brief with the bankroll's figures in its money slots, ready for the derivation. */
export function applyBankroll(brief: StrategyBrief, bankroll: Bankroll): StrategyBrief {
  return {
    ...brief,
    allocation: bankroll.allocation,
    round_share: bankroll.roundBudget === null ? brief.round_share : `$${bankroll.roundBudget}`,
    trade_share: bankroll.tradeBudget === null ? brief.trade_share : `$${bankroll.tradeBudget}`,
    stop_after_loss: bankroll.stopAfterLoss,
  }
}

export interface BankrollInputs {
  allocation: string
  roundBudget: string
  tradeBudget: string
  stopAfterLoss: string
}

/**
 * What each field shows: the typed text, or for an untouched budget the dollars the brief's
 * share works out to. A bankroll whose money does not derive shows the last typed text and
 * an empty budget, so the alert explains and the user can type.
 */
export function bankrollInputs(brief: StrategyBrief, bankroll: Bankroll): BankrollInputs {
  const derived = deriveEnvelope(applyBankroll(brief, bankroll))
  const money = derived.ok ? derived.money : undefined
  return {
    allocation: bankroll.allocation,
    roundBudget: bankroll.roundBudget ?? (money ? toInputAmount(money.roundBudget) : ''),
    tradeBudget: bankroll.tradeBudget ?? (money ? toInputAmount(money.tradeBudget) : ''),
    stopAfterLoss: bankroll.stopAfterLoss,
  }
}

/** Whole dollars and cents as tenths of a cent, the exchange's integer money. */
export function dollarsToTenths(amount: string): number {
  const [whole = '0', fraction = ''] = amount.split('.')
  const cents = Number(whole) * 100 + Number(fraction.padEnd(2, '0').slice(0, 2) || '0')
  return cents * 10
}
