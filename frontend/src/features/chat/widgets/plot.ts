'use client'

import { useEffect, useState, type RefObject } from 'react'

/** Inner width of a chat widget before the frame is measured: 24rem less padding. */
export const PLOT_FALLBACK_WIDTH = 352
/** Gutter reserved for value labels on the right of a plot. */
export const PLOT_LABEL_GUTTER = 44
/** Band under a plot for time labels. */
export const PLOT_TIME_BAND = 16
/** Baseline of the time labels, clear of the plot's bottom edge so descenders are not clipped. */
export const PLOT_TIME_BASELINE = 3

export interface PlotArea {
  left: number
  right: number
  top: number
  bottom: number
}

export function plotArea(width: number, height: number, gutter = PLOT_LABEL_GUTTER): PlotArea {
  return { left: 0, right: width - gutter, top: 4, bottom: height - PLOT_TIME_BAND }
}

/**
 * Live pixel width of the widget body, so SVG text stays crisp when the rail is
 * resized. A ResizeObserver is a real side effect, which is what useEffect is for.
 */
export function usePlotWidth(ref: RefObject<HTMLElement | null>, fallback = PLOT_FALLBACK_WIDTH) {
  const [width, setWidth] = useState(fallback)
  useEffect(() => {
    const element = ref.current
    // jsdom has no ResizeObserver; the fallback width is what tests render at.
    if (!element || typeof ResizeObserver === 'undefined') return undefined
    const observer = new ResizeObserver(entries => {
      const measured = Math.round(entries[0]?.contentRect.width ?? 0)
      if (measured > 0) setWidth(current => (current === measured ? current : measured))
    })
    observer.observe(element)
    return () => observer.disconnect()
  }, [ref])
  return width
}
