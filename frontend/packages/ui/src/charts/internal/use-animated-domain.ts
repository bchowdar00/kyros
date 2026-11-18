import { useEffect, useRef, useState } from 'react'

import { runAnimation } from '../lib/animation'

export const DOMAIN_ANIMATION_MS = 180

interface DomainState {
  domain: readonly [number, number]
  snapKey: string
}

/**
 * Eases a numeric domain towards `target` so rescaling glides instead of
 * jumping. When `snapKey` changes (for example a different chart kind or
 * value format) the domain jumps straight to the target.
 *
 * Pass `animate: false` for a domain that is already continuous, such as a live
 * window driven by the clock: easing towards a target that moves every frame
 * only adds lag and stutter.
 */
export function useAnimatedDomain(
  target: readonly [number, number],
  snapKey: string,
  animate = true,
  durationMs = DOMAIN_ANIMATION_MS
): readonly [number, number] {
  const [state, setState] = useState<DomainState>({ domain: target, snapKey })
  const currentRef = useRef(target)
  const keyRef = useRef(snapKey)
  const [targetStart, targetEnd] = target

  if (state.snapKey !== snapKey) {
    setState({ domain: target, snapKey })
  }

  useEffect(() => {
    if (!animate) {
      currentRef.current = [targetStart, targetEnd]

      return
    }

    if (keyRef.current !== snapKey) {
      keyRef.current = snapKey
      currentRef.current = [targetStart, targetEnd]

      return
    }

    const [fromStart, fromEnd] = currentRef.current

    if (fromStart === targetStart && fromEnd === targetEnd) {
      return
    }

    return runAnimation(durationMs, progress => {
      const next: readonly [number, number] = [
        fromStart + (targetStart - fromStart) * progress,
        fromEnd + (targetEnd - fromEnd) * progress,
      ]

      currentRef.current = next
      setState({ domain: next, snapKey })
    })
  }, [animate, durationMs, snapKey, targetEnd, targetStart])

  return !animate || state.snapKey !== snapKey ? target : state.domain
}
