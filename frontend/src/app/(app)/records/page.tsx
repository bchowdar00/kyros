import type { Metadata } from 'next'

import { env } from '@/env'
import { RecordBoardScreen } from '@/features/records/record-board-screen'
import { loadRecordBoard } from '@/server/record-board.server'

const description =
  'Every strategy that has sealed a round on Arc, added up from the transactions themselves.'

export const metadata: Metadata = {
  alternates: { canonical: '/records' },
  description,
  openGraph: { description, title: 'Records · Kyros', type: 'website' },
  title: 'Records',
}

// Read from the chain on request (the loader keeps a good answer for thirty seconds).
export const dynamic = 'force-dynamic'

export default async function RecordsPage() {
  return (
    <RecordBoardScreen explorerUrl={env.NEXT_PUBLIC_EXPLORER_URL} view={await loadRecordBoard()} />
  )
}
