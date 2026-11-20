import 'server-only'

import { loadVenueBoard } from '@/features/home/venue-board-loader'
import { createKeptLoader } from '@/lib/kept-loader'

/** Kalshi's prices for games move slowly before they start; a minute old is current enough. */
const FRESH_FOR_MS = 60_000
const FAILED_FOR_MS = 15_000
/** Past this a kept list may name games already played, so it is no longer shown. */
const STALE_FOR_MS = 10 * 60_000

/**
 * The games other venues price, for the Markets page. One loader per server process: every
 * visitor shares the kept list, so the page's own refresh never multiplies requests to Kalshi,
 * and a read that fails never replaces a good one.
 */
export const loadKeptVenueBoard = createKeptLoader(
  async () => {
    const board = await loadVenueBoard()
    if (board === undefined) throw new Error('no league could be read')
    return board
  },
  { failedForMs: FAILED_FOR_MS, freshForMs: FRESH_FOR_MS, staleForMs: STALE_FOR_MS }
)
