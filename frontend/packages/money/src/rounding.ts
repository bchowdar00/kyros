/**
 * Integer division with round-half-to-even (banker's rounding) on the remainder.
 * `denominator` must be positive. Works on `bigint` so intermediate products never lose
 * precision; callers convert back to `number` once the result is known to be safe.
 */
export function divideRoundHalfEven(numerator: bigint, denominator: bigint): bigint {
  if (denominator <= 0n) {
    throw new RangeError('denominator must be positive')
  }

  const quotient = numerator / denominator
  const remainder = numerator % denominator
  const twiceRemainder = remainder < 0n ? -remainder * 2n : remainder * 2n
  const isPastHalf = twiceRemainder > denominator
  const isExactlyHalf = twiceRemainder === denominator
  const quotientIsOdd = quotient % 2n !== 0n

  if (isPastHalf || (isExactlyHalf && quotientIsOdd)) {
    return numerator < 0n ? quotient - 1n : quotient + 1n
  }

  return quotient
}

export function toSafeInteger(value: bigint): number {
  if (value > BigInt(Number.MAX_SAFE_INTEGER) || value < BigInt(Number.MIN_SAFE_INTEGER)) {
    throw new RangeError(`${value} is outside the safe integer range`)
  }

  return Number(value)
}
