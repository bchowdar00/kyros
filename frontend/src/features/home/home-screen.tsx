import { LiveHomeMarketCard } from './live-home-market-card'
import type { HomeMarketCardModel } from './types'

export interface HomeScreenProps {
  cards: readonly HomeMarketCardModel[]
}

export function HomeScreen({ cards }: HomeScreenProps) {
  return (
    <main className="mx-auto w-full max-w-[90rem] px-4xl py-4xl">
      <h1 className="mb-xl font-display text-display-lg font-bold tracking-display text-foreground">
        Markets
      </h1>
      {cards.length === 0 ? (
        <p className="text-body text-muted-foreground">No live markets right now.</p>
      ) : (
        <div className="flex flex-wrap gap-xl">
          {cards.map(card => (
            <LiveHomeMarketCard card={card} key={card.slug} />
          ))}
        </div>
      )}
    </main>
  )
}
