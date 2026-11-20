import { render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import { PUBLIC_STRATEGY_FIXTURES as publicStrategiesFixture } from './public-strategies.fixture'
import { PublicStrategiesList } from './public-strategies-list'

function titles(): (string | null)[] {
  return screen.getAllByRole('article').map(card => card.querySelector('h3')?.textContent ?? null)
}

describe('PublicStrategiesList', () => {
  afterEach(() => window.history.replaceState(null, '', '/'))

  it('brings a card copied from the leaderboard to the top and says where it came from', () => {
    const last = publicStrategiesFixture.at(-1)
    if (last === undefined) throw new Error('fixture has strategies')
    window.history.replaceState(null, '', `/market/btc-5m?copy=${encodeURIComponent(last.title)}`)

    render(<PublicStrategiesList strategies={publicStrategiesFixture} />)

    expect(titles()[0]).toBe(last.title)
    expect(
      screen.getByText('Copied from the leaderboard. Run it with your own budget.')
    ).toBeVisible()
  })

  it('leaves the list as it is when nothing was copied, or the card is not on this market', () => {
    render(<PublicStrategiesList strategies={publicStrategiesFixture} />)
    const plain = titles()

    window.history.replaceState(null, '', '/market/btc-5m?copy=No+Such+Card')
    render(<PublicStrategiesList strategies={publicStrategiesFixture} />)

    expect(titles().slice(plain.length)).toEqual(plain)
    expect(screen.queryByText(/Copied from the leaderboard/)).toBeNull()
  })
})
