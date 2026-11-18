import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'

import { MarketHeader } from '@/features/market-detail/market-header'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const marketHeader = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].header
const marketSlug = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].slug

describe('MarketHeader', () => {
  it('renders the market identity and fixture-driven outcomes', () => {
    render(<MarketHeader {...marketHeader} marketSlug={marketSlug} />)

    expect(screen.getByRole('heading', { name: marketHeader.title })).toBeInTheDocument()

    // The redesigned header is title + buy buttons only: no team logos, no subtitle line.
    expect(screen.queryByRole('img', { name: /logo/i })).not.toBeInTheDocument()
    expect(screen.queryByText(marketHeader.subtitle)).not.toBeInTheDocument()

    for (const outcome of marketHeader.outcomes) {
      expect(
        screen.getByRole('button', {
          name: `Buy ${outcome.label} outcome at ${outcome.priceCents} cents`,
        })
      ).toHaveTextContent(`Buy ${outcome.label}${outcome.priceCents}¢`)
    }
  })

  it('keeps both outcome controls in the keyboard tab order', async () => {
    const user = userEvent.setup()

    render(<MarketHeader {...marketHeader} marketSlug={marketSlug} />)

    const [awayOutcome, homeOutcome] = marketHeader.outcomes
    const awayButton = screen.getByRole('button', {
      name: `Buy ${awayOutcome.label} outcome at ${awayOutcome.priceCents} cents`,
    })
    const homeButton = screen.getByRole('button', {
      name: `Buy ${homeOutcome.label} outcome at ${homeOutcome.priceCents} cents`,
    })

    await user.tab()
    expect(awayButton).toHaveFocus()

    await user.tab()
    expect(homeButton).toHaveFocus()
  })
})
