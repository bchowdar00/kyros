/**
 * A public strategy card has a title ("Inside the Spread") and two sides, each a strategy with
 * a name of its own ("Quote With the Lean", and "Quote Both Sides" for the inverse). The
 * exchange knows a running strategy only by its side's name; the site shows it under the
 * title of the card it was run from, so what a visitor clicked is what they then see.
 */
export interface StrategyCardName {
  inverseName: string
  primaryName: string
  title: string
}

/** The part of the exchange's template catalogue the names come from. */
interface CardCatalogue {
  templates: readonly {
    inverse: { name: string }
    primary: { name: string }
    title: string
  }[]
}

/** Every card of a catalogue, whether or not it can be deployed right now. */
export function toStrategyCardNames(catalogue: CardCatalogue): StrategyCardName[] {
  return catalogue.templates.map(template => ({
    inverseName: template.inverse.name,
    primaryName: template.primary.name,
    title: template.title,
  }))
}

/** A card's title for the side that runs, worded as the Run / Inverse widget heads it. */
export function cardSideTitle(title: string, inverse: boolean): string {
  return inverse ? `Inverse: ${title}` : title
}

/** The name a running strategy is shown under: its card's, or its own when it has no card. */
export function cardNameOf(strategyName: string, cards: readonly StrategyCardName[]): string {
  for (const card of cards) {
    if (card.primaryName === strategyName) return cardSideTitle(card.title, false)
    if (card.inverseName === strategyName) return cardSideTitle(card.title, true)
  }
  return strategyName
}
