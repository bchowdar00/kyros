import { type RefObject, useEffect, useRef } from 'react'

import { runAnimation } from '../lib/animation'

export const REVEAL_MS = 180

/**
 * Sweeps a clip rectangle from zero to `width` once the chart has a size so
 * the series draws in from the left. Writes straight to the DOM to avoid
 * re-renders on every frame.
 */
export function useReveal(width: number): RefObject<SVGRectElement | null> {
  const rectRef = useRef<SVGRectElement | null>(null)
  const doneRef = useRef(false)

  useEffect(() => {
    const rect = rectRef.current

    if (doneRef.current || !rect || width <= 0) {
      return
    }

    return runAnimation(REVEAL_MS, progress => {
      rect.setAttribute('width', String(Math.max(0, width * progress)))

      if (progress >= 1) {
        rect.setAttribute('width', '100000')
        doneRef.current = true
      }
    })
  }, [width])

  return rectRef
}
