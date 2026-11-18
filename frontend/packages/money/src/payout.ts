import { CENTS_PER_DOLLAR, cents, type Cents } from './cents'
import { divideRoundHalfEven, toSafeInteger } from './rounding'

/** Gross winning payout at a fixed share price, assuming fractional shares and no fees. */
export function estimateWinningPayout(stake: Cents, sharePrice: Cents): Cents {
  if (stake < 0 || sharePrice <= 0 || sharePrice > CENTS_PER_DOLLAR) {
    throw new RangeError('A payout needs a nonnegative stake and a share price from 1 to 100 cents')
  }

  return cents(
    toSafeInteger(divideRoundHalfEven(BigInt(stake) * BigInt(CENTS_PER_DOLLAR), BigInt(sharePrice)))
  )
}
