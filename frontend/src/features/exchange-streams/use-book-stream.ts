'use client'

import { useSyncExternalStore } from 'react'
import { type BookState, getBookStream, IDLE_BOOK } from './book-stream'

const getServerSnapshot = () => IDLE_BOOK
const subscribeIdle = () => () => {}

/** The series owns the connection; the midpoint line carries across renders and rounds. */
export function useBookStream(baseURL: string | undefined, series: string | undefined): BookState {
  const stream =
    baseURL !== undefined && series !== undefined && series !== ''
      ? getBookStream(baseURL, series)
      : undefined
  return useSyncExternalStore(
    stream?.subscribe ?? subscribeIdle,
    stream?.getSnapshot ?? getServerSnapshot,
    getServerSnapshot
  )
}
