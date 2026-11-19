import type { Metadata } from 'next'

import { HomeScreen } from '@/features/home/home-screen'
import { loadHomeMarkets } from '@/server/home.server'

const description = 'Live prediction markets with automated trading strategies.'

export const metadata: Metadata = {
  alternates: { canonical: '/' },
  description,
  openGraph: { description, title: 'Kyros', type: 'website' },
  title: 'Markets',
  twitter: { card: 'summary_large_image' },
}

export default async function Home() {
  return <HomeScreen cards={await loadHomeMarkets()} />
}
