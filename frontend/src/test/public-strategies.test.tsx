import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { PublicStrategiesList } from '@/features/market-detail/public-strategies-list'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import type { PublicStrategy } from '@/features/market-detail/types'

const chatPanel = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].chatPanel

function renderPublicStrategies(
  actions: { onRunStrategy?: (strategy: PublicStrategy) => void } = {}
) {
  return render(
    <PublicStrategiesList
      {...actions}
      defaultSortId={chatPanel.defaultSortId}
      sectionTitle={chatPanel.sectionTitle}
      sortLabel={chatPanel.sortLabel}
      sortOptions={chatPanel.sortOptions}
      strategies={chatPanel.publicStrategies}
    />
  )
}

describe('PublicStrategiesList', () => {
  it('distinguishes an empty catalog from an unavailable feed', async () => {
    const { container, rerender } = render(<PublicStrategiesList strategies={[]} />)
    expect(screen.getByRole('status')).toHaveTextContent('No public strategies yet')
    rerender(<PublicStrategiesList strategies={[]} availability="unavailable" />)
    expect(screen.getByRole('status')).toHaveTextContent('No public strategies to display')
    expect(screen.queryByRole('article')).not.toBeInTheDocument()
    expect(screen.getByRole('combobox')).toBeDisabled()
    await expectNoA11yViolations(container)
  })

  it('renders fixture-driven strategy details and controls', async () => {
    const { container } = renderPublicStrategies()

    expect(screen.getByRole('heading', { name: chatPanel.sectionTitle })).toBeInTheDocument()
    expect(screen.getAllByRole('article')).toHaveLength(chatPanel.publicStrategies.length)

    const firstStrategy = chatPanel.publicStrategies[0]
    const firstCard = screen.getByRole('article', { name: firstStrategy.title })

    expect(within(firstCard).getByText(`By ${firstStrategy.creator.handle}`)).toBeInTheDocument()
    expect(within(firstCard).getByText('411')).toBeInTheDocument()
    expect(within(firstCard).getByText('$2.08M')).toBeInTheDocument()
    expect(
      within(firstCard).getByRole('button', { name: `Inverse ${firstStrategy.title}` })
    ).toBeInTheDocument()
    expect(
      within(firstCard).getByRole('button', { name: `Run ${firstStrategy.title}` })
    ).toBeInTheDocument()

    // The redesign drops the per-card Edit button and the participant avatar stack.
    expect(within(firstCard).queryByRole('button', { name: /edit/i })).not.toBeInTheDocument()
    expect(within(firstCard).queryByRole('group')).not.toBeInTheDocument()

    await expectNoA11yViolations(container)
  })

  it('sorts locally by deployed value', async () => {
    const user = userEvent.setup()
    renderPublicStrategies()

    await user.selectOptions(screen.getByRole('combobox', { name: 'Sort public strategies' }), [
      'most-deployed',
    ])

    expect(screen.getAllByRole('article')[0]).toHaveAttribute(
      'data-public-strategy-id',
      'halftime-leaders-public'
    )
    expect(screen.getAllByRole('article')[1]).toHaveAttribute(
      'data-public-strategy-id',
      'momentum-flips-public'
    )
  })

  it('calls the typed Run action with the selected strategy', async () => {
    const user = userEvent.setup()
    const onRunStrategy = vi.fn()
    const firstStrategy = chatPanel.publicStrategies[0]

    renderPublicStrategies({ onRunStrategy })

    await user.click(screen.getByRole('button', { name: `Run ${firstStrategy.title}` }))

    expect(onRunStrategy).toHaveBeenCalledWith(firstStrategy)
  })
})
