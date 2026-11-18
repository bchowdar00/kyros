'use client'

import { smoothDamp, type SpringState, startFrameLoop } from './lib/animation'
import { useEffect, useState } from 'react'

export interface SmoothedNumberOptions {
  /** Skip the motion and show the value at once, for example while scrubbing. */
  immediate?: boolean
  /** Roughly how long the number takes to cover most of a change, in ms. */
  smoothTime?: number
}

interface SmoothedState {
  spring: SpringState
  target: number
}

/**
 * Rolls a displayed number towards `value` with a critically damped spring, so
 * consecutive live updates read as one continuous motion instead of a series of
 * stops and starts.
 */
export function useTweenedNumber(
  value: number,
  { immediate = false, smoothTime = 320 }: SmoothedNumberOptions = {}
) {
  const [state, setState] = useState<SmoothedState>({
    spring: { value, velocity: 0 },
    target: value,
  })

  if (state.target !== value) {
    setState(
      immediate || !Number.isFinite(value)
        ? { spring: { value, velocity: 0 }, target: value }
        : { spring: state.spring, target: value }
    )
  }

  const settled = state.spring.value === state.target && state.spring.velocity === 0

  useEffect(() => {
    if (settled) {
      return
    }

    // The loop runs until the spring settles; the effect cleanup cancels it then.
    return startFrameLoop(dt => {
      setState(current => {
        const next = smoothDamp(current.spring, current.target, smoothTime, dt)
        const scale = Math.max(1, Math.abs(current.target))
        const done =
          Math.abs(next.value - current.target) < scale * 1e-6 &&
          Math.abs(next.velocity) < scale * 1e-6

        return {
          spring: done ? { value: current.target, velocity: 0 } : next,
          target: current.target,
        }
      })

      return true
    })
  }, [settled, smoothTime])

  return state.target === value ? state.spring.value : value
}
