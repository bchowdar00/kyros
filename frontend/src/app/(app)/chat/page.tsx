import type { Metadata } from 'next'

import { ChatPlaygroundScreen } from '@/features/chat/chat-playground-screen'

export const metadata: Metadata = {
  description: 'Chat widget frames at the size they render in the market chat rail.',
  // A design gallery, not a destination; it stays unindexed like /charts.
  robots: { follow: false, index: false },
  title: 'Chat',
}

export default function ChatPage() {
  return <ChatPlaygroundScreen />
}
