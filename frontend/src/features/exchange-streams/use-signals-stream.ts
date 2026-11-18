'use client'

import { useEffect, useSyncExternalStore } from 'react'

import {
  getSignalsStream,
  IDLE_SIGNALS,
  type SignalsRequest,
  type SignalsState,
} from './signals-stream'

const getServerSnapshot = () => IDLE_SIGNALS

/**
 * Subscribe to a series' signals socket with what this consumer wants. `ids` are joined
 * into one string so the request is re-registered only when its content changes.
 */
export function useSignalsStream(
  apiUrl: string,
  series: string,
  ids: readonly string[] | 'series',
  historyMs?: number
): SignalsState {
  const stream = getSignalsStream(apiUrl, series)
  const idsKey = ids === 'series' ? 'series' : ids.join(',')
  useEffect(() => {
    const request: SignalsRequest = {
      ids: idsKey === 'series' ? 'series' : idsKey === '' ? [] : idsKey.split(','),
      ...(historyMs === undefined ? {} : { historyMs }),
    }
    return stream.register(request)
  }, [stream, idsKey, historyMs])
  return useSyncExternalStore(stream.subscribe, stream.getSnapshot, getServerSnapshot)
}
