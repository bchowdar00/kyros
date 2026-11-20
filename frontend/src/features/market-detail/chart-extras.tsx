'use client'

import type { TradeMark } from '@kyros/ui/charts/trade-marks'
import type { VenueLine } from '@kyros/ui/charts/venue-lines'
import { createContext, useContext, useEffect, useMemo, useSyncExternalStore } from 'react'

import {
  getSignalsStream,
  HISTORY_WINDOW_MS,
  type SignalsStream,
} from '@/features/exchange-streams/signals-stream'

import type { SignalHistory } from '@/features/signals/signal-history'

import { extendVenueHistory, venueLinesFor, venueSignalIds } from './venue-lines'

const NO_MARKS: readonly TradeMark[] = []
const NO_LINES: readonly VenueLine[] = []
const noLines = () => NO_LINES
const neverChanges = () => () => undefined
const ignore = () => undefined

const TradeMarksContext = createContext<readonly TradeMark[]>(NO_MARKS)

/** Hands the account's orders to whichever chart the market screen renders. */
export const TradeMarksProvider = TradeMarksContext.Provider

/** The orders to mark on this market's chart; none outside a connected market screen. */
export function useTradeMarks(): readonly TradeMark[] {
  return useContext(TradeMarksContext)
}

/**
 * A venue's readings held for `keepMs`, longer than the shared socket keeps them, and the
 * lines drawn from them. It follows the socket's once-a-second snapshot and never asks the
 * socket for a longer window, because every signal tile on the page draws from that window.
 */
function createHeldLines(
  stream: SignalsStream,
  series: string,
  keepMs: number,
  past: SignalHistory | undefined
) {
  // Readings from before the page opened, when the server read them, start each line.
  let held: SignalHistory = past === undefined ? {} : extendVenueHistory({}, past, series, keepMs)
  let lines: readonly VenueLine[] = past === undefined ? NO_LINES : venueLinesFor(series, held)
  const take = () => {
    const next = extendVenueHistory(held, stream.getCalmSnapshot().history, series, keepMs)
    if (next === held) return false
    held = next
    lines = venueLinesFor(series, held)
    return true
  }
  return {
    getSnapshot: () => lines,
    subscribe(listener: () => void) {
      take()
      return stream.subscribeCalm(() => {
        if (take()) listener()
      })
    },
  }
}

/**
 * Other venues' prices for a series, as lines for its chart, read from the
 * page's shared signals socket at its once-a-second pace. Nothing without a
 * server, or for a series no other venue prices. `keepMs` is how far back a
 * line reaches, for a chart that spans more than the socket's own minute; `past` is what
 * the server read of the time before the page opened.
 */
export function useVenueLines(
  apiUrl: string | undefined,
  series: string | undefined,
  keepMs: number = HISTORY_WINDOW_MS,
  past?: SignalHistory
): readonly VenueLine[] {
  const ids = series === undefined ? '' : venueSignalIds(series).join(',')
  const stream =
    apiUrl !== undefined && series !== undefined && ids !== ''
      ? getSignalsStream(apiUrl, series)
      : undefined

  useEffect(() => {
    if (stream === undefined) return undefined
    const withdraw = stream.register({ ids: ids.split(',') })
    // The calm snapshot is only refreshed while someone listens to the stream itself.
    const unsubscribe = stream.subscribe(ignore)
    return () => {
      unsubscribe()
      withdraw()
    }
  }, [stream, ids])

  const lines = useMemo(
    () =>
      stream === undefined || series === undefined
        ? undefined
        : createHeldLines(stream, series, keepMs, past),
    [stream, series, keepMs, past]
  )

  return useSyncExternalStore(
    lines?.subscribe ?? neverChanges,
    lines?.getSnapshot ?? noLines,
    noLines
  )
}
