import 'server-only'

import { env } from '@/env'
import { createRecordBoardLoader } from '@/features/records/record-board-loader'

/**
 * The record board, read from the contract's transactions on Arc. One loader for the whole
 * server, so its thirty-second answer is shared by every visitor.
 */
export const loadRecordBoard = createRecordBoardLoader({
  contract: env.RECORD_CONTRACT_ADDRESS,
  exchangeUrl: env.EXCHANGE_API_URL,
  explorerUrl: env.NEXT_PUBLIC_EXPLORER_URL,
})
