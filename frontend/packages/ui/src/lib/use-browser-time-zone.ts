'use client'

import { useSyncExternalStore } from 'react'

const subscribe = () => () => {}
let browserTimeZone: string | undefined

function getBrowserTimeZone() {
  browserTimeZone ??= new Intl.DateTimeFormat().resolvedOptions().timeZone
  return browserTimeZone
}

/** Use the browser's zone after hydration, without an IP lookup or location permission. */
export function useBrowserTimeZone(serverTimeZone = 'UTC'): string {
  return useSyncExternalStore(subscribe, getBrowserTimeZone, () => serverTimeZone)
}
