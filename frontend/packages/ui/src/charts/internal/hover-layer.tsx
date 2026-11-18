'use client'

import { type FocusEvent, type KeyboardEvent, type PointerEvent, useEffect, useRef } from 'react'

import { findNearestIndex } from '../lib/scale'
import { useChartContext } from './chart-context'
import { Crosshair } from './crosshair'
import { buildHover, describeHover, getPrimarySamples } from './hover-model'
import { type HoverSource, useHoverState } from './hover-store'

function isFocusVisible(element: Element) {
  try {
    return element.matches(':focus-visible')
  } catch {
    return true
  }
}

export interface HoverLayerProps {
  label: string
}

/** Transparent hit area that owns pointer, touch and keyboard scrubbing. */
export function HoverLayer({ label }: HoverLayerProps) {
  const { geometry, hover } = useChartContext()
  const state = useHoverState(hover)
  const { dataEndX, plot, range, xScale } = geometry
  const samples = getPrimarySamples(geometry)
  const rectRef = useRef<SVGRectElement | null>(null)

  // When the samples change under the cursor (a kind switch, a reset) the stored
  // index no longer points at the same moment, so the crosshair must go.
  useEffect(() => {
    const current = hover.get()

    if (current && samples[findNearestIndex(samples, current.time)]?.time !== current.time) {
      hover.set(null)
    }
  }, [hover, samples])

  const commit = (index: number, source: HoverSource) => {
    const sample = samples[index]

    if (!sample) {
      hover.set(null)

      return
    }

    hover.set({ index, source, time: sample.time, x: xScale(sample.time) })
  }

  const handlePointer = (event: PointerEvent<SVGRectElement>) => {
    const rect = rectRef.current?.ownerSVGElement?.getBoundingClientRect()

    if (!rect || samples.length === 0) {
      return
    }

    const scale = rect.width > 0 ? geometry.width / rect.width : 1
    const x = Math.min(Math.max((event.clientX - rect.left) * scale, plot.left), dataEndX)

    commit(findNearestIndex(samples, xScale.invert(x)), 'pointer')
  }

  const clear = () => {
    hover.set(null)
  }

  const handleKeyDown = (event: KeyboardEvent<SVGRectElement>) => {
    if (samples.length === 0) {
      return
    }

    const current = state ? findNearestIndex(samples, state.time) : samples.length - 1
    const step = event.shiftKey ? Math.max(1, Math.round(samples.length / 20)) : 1
    let next: number | null = null

    switch (event.key) {
      case 'ArrowLeft':
      case 'ArrowDown':
        next = Math.max(0, current - step)
        break
      case 'ArrowRight':
      case 'ArrowUp':
        next = Math.min(samples.length - 1, current + step)
        break
      case 'Home':
        next = 0
        break
      case 'End':
        next = samples.length - 1
        break
      case 'Escape':
        clear()
        event.preventDefault()

        return
      default:
        return
    }

    event.preventDefault()
    commit(next, 'keyboard')
  }

  const handleFocus = (event: FocusEvent<SVGRectElement>) => {
    if (!state && isFocusVisible(event.target)) {
      commit(samples.length - 1, 'keyboard')
    }
  }

  const hoverDetails = state ? buildHover(geometry, state) : null

  return (
    <>
      {hoverDetails && state ? <Crosshair hover={hoverDetails} state={state} /> : null}
      <rect
        aria-label={label}
        aria-orientation="horizontal"
        aria-valuemax={range.end}
        aria-valuemin={range.start}
        aria-valuenow={state?.time ?? samples.at(-1)?.time ?? range.end}
        aria-valuetext={hoverDetails ? describeHover(geometry, hoverDetails) : undefined}
        className="cursor-crosshair focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-foreground"
        data-chart-hit-area=""
        fill="transparent"
        height={plot.height}
        onBlur={clear}
        onFocus={handleFocus}
        onKeyDown={handleKeyDown}
        onPointerCancel={clear}
        onPointerDown={handlePointer}
        onPointerLeave={clear}
        onPointerMove={handlePointer}
        ref={rectRef}
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role -- SVG has no native range input
        role="slider"
        style={{ touchAction: 'pan-y' }}
        tabIndex={0}
        width={plot.width}
        x={plot.left}
        y={plot.top}
      />
    </>
  )
}

/** Announces keyboard scrubbing to screen readers without narrating pointer moves. */
export function HoverAnnouncer() {
  const { geometry, hover } = useChartContext()
  const state = useHoverState(hover)
  const text =
    state?.source === 'keyboard' ? describeHover(geometry, buildHover(geometry, state)) : ''

  return (
    <div aria-live="polite" className="sr-only">
      {text}
    </div>
  )
}
