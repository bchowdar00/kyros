import { nbaTeam } from '@/features/nba-market/teams'

import type { HomeMarketCardModel, MarketCardArtwork } from './types'

const bitcoinArtwork: MarketCardArtwork = {
  images: [{ src: '/assets/market/bitcoin-network.svg' }],
  label: 'Bitcoin mark',
}

/** An NBA card shows the two teams' marks, away then home. */
function nbaArtwork(card: HomeMarketCardModel): MarketCardArtwork {
  const [away, home] = [
    card.outcomes.find(outcome => outcome.side === 'away'),
    card.outcomes.find(outcome => outcome.side === 'home'),
  ]
  const images = [away, home]
    .filter(outcome => outcome !== undefined)
    .map(outcome => {
      const team = nbaTeam(outcome.label)
      return {
        src: `/assets/market/teams/${team?.logo ?? 'nba'}.svg`,
        ...(outcome.label === 'LAL'
          ? { presentation: 'inset' as const, tone: 'lakers' as const }
          : {}),
      }
    })
  return { images, label: `${away?.label ?? ''} at ${home?.label ?? ''}` }
}

export function getHomeMarketCardVisualConfig(card: HomeMarketCardModel): MarketCardArtwork {
  return card.category === 'NBA' ? nbaArtwork(card) : bitcoinArtwork
}
