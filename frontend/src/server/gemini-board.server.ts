import 'server-only'

import { env } from '@/env'
import { createGeminiBoardLoader } from '@/features/home/gemini-board-loader'

/**
 * Gemini's up/down series and games for the Markets page. One loader per server process, so
 * every visitor shares the same kept lists.
 */
export const loadGeminiBoard = createGeminiBoardLoader({ apiUrl: env.GEMINI_API_URL })
