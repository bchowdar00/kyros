import { describe, expect, it } from 'vitest'

import { cardNameOf, toStrategyCardNames } from './strategy-card-names'

const catalogue = {
  templates: [
    {
      title: 'Inside the Spread',
      primary: { name: 'Quote With the Lean' },
      inverse: { name: 'Quote Both Sides' },
      // Extra fields, and a card that is waiting on a signal, make no difference to its name.
      ready: false,
    },
    {
      title: 'Hot Hand',
      primary: { name: 'Hot Hand' },
      inverse: { name: 'Cold Hand' },
    },
  ],
}

describe('the name a running strategy is shown under', () => {
  const cards = toStrategyCardNames(catalogue)

  it('is the title of the card it was run from', () => {
    expect(cardNameOf('Quote With the Lean', cards)).toBe('Inside the Spread')
    expect(cardNameOf('Hot Hand', cards)).toBe('Hot Hand')
  })

  it('says so when it is the card’s inverse', () => {
    expect(cardNameOf('Quote Both Sides', cards)).toBe('Inverse: Inside the Spread')
    expect(cardNameOf('Cold Hand', cards)).toBe('Inverse: Hot Hand')
  })

  it('is its own name when it came from no card', () => {
    expect(cardNameOf('Two Minute Lead Buy', cards)).toBe('Two Minute Lead Buy')
    expect(cardNameOf('Quote With the Lean', [])).toBe('Quote With the Lean')
  })
})
