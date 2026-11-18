import type { Brand } from './brand'
import { CENTS_PER_DOLLAR, cents, type Cents } from './cents'
import { divideRoundHalfEven, toSafeInteger } from './rounding'

/**
 * Basis points: one hundredth of a percent as an integer. 10,000 bps is 100%.
 * Mirrors the backend's `*_bps` fields for probabilities, fees, and ratios.
 */
export type Bps = Brand<number, 'Bps'>

export const BPS_PER_UNIT = 10_000

export function bps(value: number): Bps {
  if (!Number.isSafeInteger(value)) {
    throw new RangeError(`Bps must be a safe integer, received ${String(value)}`)
  }

  return value as Bps
}

/** A probability is a ratio clamped to the closed interval [0, 10,000]. */
export function probabilityBps(value: number): Bps {
  const ratio = bps(value)

  if (ratio < 0 || ratio > BPS_PER_UNIT) {
    throw new RangeError(
      `A probability must be between 0 and ${BPS_PER_UNIT} bps, received ${value}`
    )
  }

  return ratio
}

/** Applies a ratio to an amount with banker's rounding: fees, shares of a total, discounts. */
export function applyBps(amount: Cents, ratio: Bps): Cents {
  const product = BigInt(amount) * BigInt(ratio)

  return cents(toSafeInteger(divideRoundHalfEven(product, BigInt(BPS_PER_UNIT))))
}

/**
 * One share pays $1.00 if its outcome happens, so a price in cents *is* the market's
 * probability. 31¢ is 3,100 bps. The conversion is exact.
 */
export function priceCentsToProbabilityBps(price: Cents): Bps {
  return probabilityBps(price * (BPS_PER_UNIT / CENTS_PER_DOLLAR))
}

/** The inverse of {@link priceCentsToProbabilityBps}. Sub-cent probabilities round half-even. */
export function probabilityBpsToPriceCents(probability: Bps): Cents {
  const bpsPerCent = BigInt(BPS_PER_UNIT / CENTS_PER_DOLLAR)

  return cents(toSafeInteger(divideRoundHalfEven(BigInt(probability), bpsPerCent)))
}
