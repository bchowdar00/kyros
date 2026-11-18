import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'

import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { SignalsSection } from '@/features/market-detail/signals-section'

const signals = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].signals

describe('SignalsSection', () => {
  it('renders every fixture-backed signal in a horizontal list', () => {
    render(<SignalsSection {...signals} />)

    const section = screen.getByRole('region', { name: signals.title })
    const list = within(section).getByRole('list')

    expect(within(list).getAllByRole('listitem')).toHaveLength(signals.items.length)

    for (const signal of signals.items) {
      const label = within(list).getByText(signal.label)
      const card = label.closest('article')

      expect(card).not.toBeNull()
      expect(within(card as HTMLElement).getByText(signal.value)).toBeInTheDocument()
    }

    expect(within(section).getByRole('region', { name: `${signals.title} list` })).toHaveClass(
      'overflow-x-auto'
    )
  })

  it('keeps the browse control in the keyboard tab order', async () => {
    const user = userEvent.setup()

    render(<SignalsSection {...signals} />)

    const browseButton = screen.getByRole('button', {
      name: `${signals.browseLabel} (${signals.total} signals)`,
    })
    await user.tab()
    expect(browseButton).toHaveFocus()
  })
})
