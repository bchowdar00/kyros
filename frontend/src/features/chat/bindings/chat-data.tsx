'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createContext, type ReactNode, useContext, useState } from 'react'

/** Where the widgets in a conversation read from: one exchange, one series, one account. */
export interface ChatData {
  apiUrl: string
  series: string
  /** The identity the user trades as; `null` renders account widgets as unavailable. */
  address: string | null
  timeZone: string
}

const ChatDataContext = createContext<ChatData | null>(null)

/**
 * Mounted by the chat panel on a market backed by the exchange. Widgets outside it, such
 * as the gallery, have nothing live to bind to and say so.
 */
export function ChatDataProvider({ value, children }: { value: ChatData; children: ReactNode }) {
  const [queryClient] = useState(() => new QueryClient())
  return (
    <QueryClientProvider client={queryClient}>
      <ChatDataContext.Provider value={value}>{children}</ChatDataContext.Provider>
    </QueryClientProvider>
  )
}

export function useChatData(): ChatData | null {
  return useContext(ChatDataContext)
}
