import { useEffect, useState } from 'react'

import { canAnimate, smoothDamp, type SpringState, startFrameLoop } from '../lib/animation'
import type { ChartPoint, LineSeries } from '../lib/types'

/** Roughly the time the tail takes to cover most of a tick, in ms. Tuned for 250–1000ms feeds. */
export const LIVE_SMOOTH_MS = 180

interface Tail {
  /** Number of points the series had when the target was captured. */
  length: number
  target: ChartPoint
  time: SpringState
  value: SpringState
}

interface LiveState {
  display: readonly LineSeries[]
  settled: boolean
  source: readonly LineSeries[]
  tails: ReadonlyMap<string, Tail>
}

function restingTail(point: ChartPoint, length: number): Tail {
  return {
    length,
    target: point,
    time: { value: point.time, velocity: 0 },
    value: { value: point.value, velocity: 0 },
  }
}

function tailPoint(tail: Tail): ChartPoint {
  const point: ChartPoint = { time: tail.time.value, value: tail.value.value }

  if (tail.target.volume !== undefined) {
    point.volume = tail.target.volume
  }

  return point
}

function isSettled(tail: Tail) {
  return (
    tail.time.value === tail.target.time &&
    tail.value.value === tail.target.value &&
    tail.time.velocity === 0 &&
    tail.value.velocity === 0
  )
}

function applyTails(series: readonly LineSeries[], tails: ReadonlyMap<string, Tail>) {
  return series.map(item => {
    const tail = tails.get(item.id)

    if (!tail || isSettled(tail)) {
      return item
    }

    const head = item.points.slice(0, -1)
    const point = tailPoint(tail)
    const previous = head.at(-1)

    // The tail lags the newest sample, and a lag that reaches back past the
    // sample before it would draw the line briefly going backwards in time.
    return {
      ...item,
      points: [
        ...head,
        previous && point.time <= previous.time ? { ...point, time: previous.time } : point,
      ],
    }
  })
}

/** Decides, for a new `series` value, which tails keep chasing and which snap. */
function nextState(
  previous: LiveState,
  series: readonly LineSeries[],
  enabled: boolean
): LiveState {
  const tails = new Map<string, Tail>()

  for (const item of series) {
    const first = item.points[0]
    const last = item.points.at(-1)

    if (!first || !last) {
      continue
    }

    // An append keeps covering the moment the previous tip stood at; a history that
    // starts after it is a reset and snaps. The tip's own time need not survive: a
    // provisional tip is replaced by the grid point that commits behind it.
    const before = previous.tails.get(item.id)
    const appended =
      enabled &&
      canAnimate() &&
      before !== undefined &&
      last.time >= before.target.time &&
      first.time <= before.target.time

    tails.set(
      item.id,
      appended && before
        ? { length: item.points.length, target: last, time: before.time, value: before.value }
        : restingTail(last, item.points.length)
    )
  }

  const settled = [...tails.values()].every(isSettled)

  return { display: settled ? series : applyTails(series, tails), settled, source: series, tails }
}

const VALUE_EPSILON = 1e-6

function stepTail(tail: Tail, dt: number): Tail {
  const time = smoothDamp(tail.time, tail.target.time, LIVE_SMOOTH_MS, dt)
  const value = smoothDamp(tail.value, tail.target.value, LIVE_SMOOTH_MS, dt)
  const valueScale = Math.max(1, Math.abs(tail.target.value))
  const converged =
    Math.abs(time.value - tail.target.time) < 1 &&
    Math.abs(time.velocity) < 0.001 &&
    Math.abs(value.value - tail.target.value) < valueScale * VALUE_EPSILON &&
    Math.abs(value.velocity) < valueScale * VALUE_EPSILON

  return converged ? restingTail(tail.target, tail.length) : { ...tail, time, value }
}

/**
 * When `enabled`, a freshly appended sample is chased by a critically damped
 * spring, so the tail keeps moving smoothly through consecutive ticks instead of
 * easing to a stop and jumping. Anything other than an append snaps.
 */
export function useLiveSeries(
  series: readonly LineSeries[],
  enabled: boolean
): readonly LineSeries[] {
  const [state, setState] = useState<LiveState>(() =>
    nextState({ display: series, settled: true, source: series, tails: new Map() }, series, false)
  )

  if (state.source !== series) {
    setState(nextState(state, series, enabled))
  }

  const { settled, source } = state

  useEffect(() => {
    if (settled) {
      return
    }

    // The loop runs until the state settles; the effect cleanup cancels it then.
    return startFrameLoop(dt => {
      setState(current => {
        if (current.source !== source || current.settled) {
          return current
        }

        const tails = new Map<string, Tail>()

        for (const [id, tail] of current.tails) {
          tails.set(id, isSettled(tail) ? tail : stepTail(tail, dt))
        }

        const nowSettled = [...tails.values()].every(isSettled)

        return {
          display: nowSettled ? source : applyTails(source, tails),
          settled: nowSettled,
          source,
          tails,
        }
      })

      return true
    })
  }, [settled, source])

  return state.source === series ? state.display : series
}
