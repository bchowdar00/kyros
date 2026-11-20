import type { Metadata } from 'next'

import { env } from '@/env'
import { RecordBoardScreen } from '@/features/records/record-board-screen'
import { loadRecordBoard } from '@/server/record-board.server'

const description =
  'The best strategies by sealed result, added up from their transactions on Arc. Copy a public one.'

export const metadata: Metadata = {
  alternates: { canonical: '/leaderboard' },
  description,
  openGraph: { description, title: 'Leaderboard · Kyros', type: 'website' },
  title: 'Leaderboard',
}

// Read from the chain on request (the loader keeps a good answer for thirty seconds).
export const dynamic = 'force-dynamic'

export default async function LeaderboardPage() {
  return (
    <RecordBoardScreen explorerUrl={env.NEXT_PUBLIC_EXPLORER_URL} view={await loadRecordBoard()} />
  )
}
