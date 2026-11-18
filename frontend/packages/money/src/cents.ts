import type { Brand } from './brand'

/**
 * USD in integer minor units. 100 Cents is $1.00. Mirrors the backend's `*_cents` fields.
 * Negative values are allowed (P&L, deltas); construction rejects anything that is not a
 * safe integer, so a float can never become a Cents value.
 */
export type Cents = Brand<number, 'Cents'>

export const CENTS_PER_DOLLAR = 100

export function cents(value: number): Cents {
  if (!Number.isSafeInteger(value)) {
    throw new RangeError(`Cents must be a safe integer, received ${String(value)}`)
  }

  return value as Cents
}

export function isCents(value: unknown): value is Cents {
  return typeof value === 'number' && Number.isSafeInteger(value)
}

export const ZERO_CENTS = cents(0)

export function addCents(left: Cents, right: Cents): Cents {
  return cents(left + right)
}

export function subtractCents(left: Cents, right: Cents): Cents {
  return cents(left - right)
}

export function sumCents(values: Iterable<Cents>): Cents {
  let total = 0

  for (const value of values) {
    total += value
  }

  return cents(total)
}

export function compareCents(left: Cents, right: Cents): -1 | 0 | 1 {
  if (left < right) {
    return -1
  }

  return left > right ? 1 : 0
}
