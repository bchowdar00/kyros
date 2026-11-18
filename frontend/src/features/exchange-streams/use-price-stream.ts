'use client'

import { useSyncExternalStore } from 'react'
import { getPriceStream, IDLE_PRICE_STREAM, type PriceStreamState } from './price-stream'

const getServerSnapshot = () => IDLE_PRICE_STREAM
const subscribeIdle = () => () => {}

/** The symbol owns the connection and history; changing market IDs never resets either. */
export function usePriceStream(
  baseURL: string | undefined,
  symbol: string | undefined
): PriceStreamState {
  const stream =
    baseURL !== undefined && symbol !== undefined && symbol !== ''
      ? getPriceStream(baseURL, symbol)
      : undefined
  return useSyncExternalStore(
    stream?.subscribe ?? subscribeIdle,
    stream?.getSnapshot ?? getServerSnapshot,
    getServerSnapshot
  )
}
