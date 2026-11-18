import { useSyncExternalStore } from 'react'

import type { SharedStream } from '@/features/exchange-streams/shared-stream'

/**
 * What a binding hands the renderer. `stale` still carries props: the last known values
 * with the frame saying the feed dropped. `unavailable` always says why (AGENTS.md §0.5).
 */
export type Binding<Props> =
  | { status: 'loading' }
  | { status: 'live' | 'stale'; props: Props }
  | { status: 'unavailable'; reason: string }

export const LOADING: Binding<never> = { status: 'loading' }

export function unavailable(reason: string): Binding<never> {
  return { status: 'unavailable', reason }
}

/** Read a shared stream; the server snapshot is the same idle value the store starts with. */
export function useStream<Snapshot>(stream: SharedStream<Snapshot>, idle: Snapshot): Snapshot {
  return useSyncExternalStore(stream.subscribe, stream.getSnapshot, () => idle)
}

export { seriesSignalId } from '../resolve/series'

/** Milliseconds left in the round from the window signal's latest frame. */
export function remainingMs(values: Record<string, string | number | boolean> | undefined) {
  const raw = values?.['remaining']
  return typeof raw === 'number' && Number.isFinite(raw) ? raw : undefined
}
