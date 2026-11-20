import { cents, type Cents } from '@kyros/money'

import type { AccountState } from './account'

/** The exchange refuses a sub-account under $10 (`MIN_SUB_ACCOUNT_ALLOCATION`). */
export const MIN_ALLOCATION_CENTS = cents(1_000)
/** A dollar is 1,000 tenths of a cent on the wire, so a cent is ten. */
const TENTHS_PER_CENT = 10

export interface StrategyDeployRequest {
  templateId: string
  inverse: boolean
  allocationCents: Cents
  /** What the visitor ran, as the card words it; the exchange's own name is the fallback. */
  title?: string
}

/** The notice after a deployment: the card it was run from, and what it is doing now. */
export function deployedNotice(
  title: string | undefined,
  exchangeName: string,
  leg: string | undefined
): string {
  return `Deployed ${title ?? exchangeName}${leg === undefined ? '' : `, now ${leg}`}.`
}

/** What the Run / Inverse widget needs to deploy a template into a funded sub-account. */
export interface StrategyDeploy {
  availableBalanceCents: Cents | undefined
  /** Why deploying is disabled right now; absent when it is allowed. */
  reason: string | undefined
  /** The outcome of the last deploy attempt. */
  notice: string
  /** Resolves true when the exchange accepted the deployment. */
  submit: (request: StrategyDeployRequest) => Promise<boolean>
}

/** A dollars-and-cents input (`100`, `25.5`) to exact cents; no float intermediate. */
export function allocationCents(amount: string): Cents | undefined {
  if (amount.length > 16 || !/^\d+(?:\.\d{0,2})?$/.test(amount)) return undefined
  const [whole = '0', fraction = ''] = amount.split('.')
  const minorUnits = BigInt(whole) * 100n + BigInt(fraction.padEnd(2, '0'))
  return minorUnits <= BigInt(Number.MAX_SAFE_INTEGER) ? cents(Number(minorUnits)) : undefined
}

export function allocationTenths(amount: Cents): number {
  return amount * TENTHS_PER_CENT
}

/** Why the account cannot deploy right now, before an allocation is considered. */
export function deployUnavailable(account: AccountState, pending: boolean): string | undefined {
  if (pending) return 'Deploying. Waiting for the exchange.'
  if (account.status !== 'live' || account.available === null)
    return 'Waiting for the exchange account snapshot.'
  return undefined
}

/**
 * Why the template cannot deploy while the exchange is waiting on its signals: a deployment
 * made now would never fire and never say why.
 */
export function signalsUnavailable(waitingOn: readonly string[] | undefined): string | undefined {
  if (waitingOn === undefined || waitingOn.length === 0) return undefined
  const names = new Intl.ListFormat('en-US', { type: 'conjunction' }).format(waitingOn)
  return `Waiting on ${names}. Deploying now would leave the strategy idle.`
}

/** Why this allocation cannot be deployed, given the available balance. */
export function allocationUnavailable(
  amount: string,
  availableBalanceCents: Cents | undefined
): string | undefined {
  const allocation = allocationCents(amount)
  if (allocation === undefined || allocation < MIN_ALLOCATION_CENTS)
    return 'Enter an allocation of at least $10.'
  if (availableBalanceCents !== undefined && allocation > availableBalanceCents)
    return 'Allocation exceeds your available balance.'
  return undefined
}
