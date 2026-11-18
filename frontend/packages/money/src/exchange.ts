import { cents, type Cents } from './cents'
import { divideRoundHalfEven, toSafeInteger } from './rounding'

/** Gross winning payout assuming fractional shares, before fees. */
export function estimateExchangeWinningPayout(stake: Cents, priceTicks: number): Cents {
  if (stake < 0 || !Number.isSafeInteger(priceTicks) || priceTicks <= 0 || priceTicks > 1000)
    throw new RangeError('Invalid exchange payout quote')
  return cents(toSafeInteger(divideRoundHalfEven(BigInt(stake) * 1000n, BigInt(priceTicks))))
}

/** Exact display adapters for the exchange's finer fixed-point wire units. */
export function formatExchangeTicks(ticks: number): string {
  if (!Number.isSafeInteger(ticks) || ticks < 0 || ticks > 1000)
    throw new RangeError('Invalid ticks')
  return `${Math.trunc(ticks / 10)}.${ticks % 10}¢`
}

/** Spreads can be negative while separate bid/ask updates are arriving. */
export function formatExchangeSpread(ticks: number): string {
  return `${ticks < 0 ? '-' : ''}${formatExchangeTicks(Math.abs(ticks))}`
}
export function formatExchangeBalance(units: string): string {
  const micros = BigInt(units)
  if (micros < 0n) throw new RangeError('Invalid balance')
  return `$${micros / 1000000n}.${(micros % 1000000n).toString().padStart(6, '0')}`
}

/** USD input to exact micro-dollars; no float intermediate. */
export function exchangeBudgetMicros(amount: string): bigint | undefined {
  if (amount.length > 16 || !/^\d+(?:\.\d{0,2})?$/.test(amount)) return undefined
  const [whole = '0', fraction = ''] = amount.split('.')
  const micros = BigInt(whole) * 1000000n + BigInt(fraction.padEnd(6, '0'))
  return micros > 0n ? micros : undefined
}

/** Per-lot admission cap for buying either outcome; mirrors gateway risk/fees.rs. */
export function exchangeBuyReserve(priceTicks: number, feeRateBps: number): bigint {
  if (
    !Number.isInteger(priceTicks) ||
    priceTicks < 1 ||
    priceTicks > 999 ||
    !Number.isInteger(feeRateBps) ||
    feeRateBps < 0 ||
    feeRateBps > 1000
  )
    throw new RangeError('Invalid exchange quote')
  const worstTick = BigInt(Math.min(priceTicks, 500))
  const fee = ((BigInt(feeRateBps) * worstTick * (1000n - worstTick) + 99999n) / 100000n) * 10n
  return BigInt(priceTicks) * 1000n + fee
}
