import type { Metadata } from 'next'
import { notFound } from 'next/navigation'

import { ReplayIndexSchema } from '@/features/replay/replay'
import indexData from '@/features/replay/replay-index.json'
import { ReplayScreen } from '@/features/replay/replay-screen'

type ReplayPageProps = PageProps<'/replay/[id]'>

const index = ReplayIndexSchema.parse(indexData)

/** The index entry for a game id, with its position in the index, or undefined. */
function findReplay(id: string) {
  const position = index.findIndex(entry => entry.id === id)
  const entry = index[position]
  if (position === -1 || entry === undefined) return undefined
  return { entry, index: position }
}

export async function generateMetadata({ params }: ReplayPageProps): Promise<Metadata> {
  const { id } = await params
  const found = findReplay(id)
  if (!found) return {}
  return { title: `${found.entry.away.code} at ${found.entry.home.code} replay` }
}

export default async function ReplayPage({ params }: ReplayPageProps) {
  const { id } = await params
  const found = findReplay(id)
  if (!found) notFound()
  return <ReplayScreen entry={found.entry} index={found.index} />
}
