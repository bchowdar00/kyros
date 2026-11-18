import { useState } from 'react'

/** Hold the viewport through ordinary ticks; shrink only after a large excursion has left. */
export function useStableDomain(
  target: readonly [number, number],
  key: string,
  enabled: boolean
): readonly [number, number] {
  const [held, setHeld] = useState({ domain: target, key })
  const [low, high] = target
  const [heldLow, heldHigh] = held.domain
  const reset = held.key !== key || !enabled
  const expands = low < heldLow || high > heldHigh
  const contracts = high - low < (heldHigh - heldLow) * 0.45
  if (reset || expands || contracts) {
    const domain: readonly [number, number] =
      reset || contracts ? target : [Math.min(low, heldLow), Math.max(high, heldHigh)]
    if (held.key !== key || domain[0] !== heldLow || domain[1] !== heldHigh) {
      setHeld({ domain, key })
    }
    return domain
  }
  return held.domain
}
