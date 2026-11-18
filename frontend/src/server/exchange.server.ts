import 'server-only'

import { env } from '@/env'
import type { RustExchangeConfigProps } from '@/features/market-detail/exchange/rust-market-screen'

/** The browser trades directly against the Rust exchange; the page only needs its origin. */
export function loadExchangeConfig(series: string): RustExchangeConfigProps | undefined {
  return env.NEXT_PUBLIC_API_URL === undefined
    ? undefined
    : { apiUrl: env.NEXT_PUBLIC_API_URL, series }
}
