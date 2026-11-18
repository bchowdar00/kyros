import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

import { MarketDetailScreen } from '@/features/market-detail/market-detail-screen'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { StrategyComposer } from '@/features/market-detail/strategy-composer'
import { ChatPanel } from '@/features/market-detail/chat-panel'

const market = MARKET_DETAIL_FIXTURES['nba-bos-la-083126']

describe('ChatPanel', () => {
  it('composes its header, public-strategy list and composer without a close control', () => {
    render(<ChatPanel data={market.chatPanel} />)

    expect(screen.getByRole('heading', { name: market.chatPanel.title })).toBeInTheDocument()
    expect(screen.getByText(market.chatPanel.subtitle)).toBeInTheDocument()
    expect(screen.getByRole('heading', { name: market.chatPanel.sectionTitle })).toBeInTheDocument()
    expect(screen.getByRole('form', { name: 'Strategy composer' })).toBeInTheDocument()

    // The redesigned rail panel has no close affordance.
    expect(screen.queryByRole('button', { name: /close/i })).not.toBeInTheDocument()
  })

  it('scrolls the strategy list inside the panel with the composer floating above it', () => {
    render(<MarketDetailScreen market={market} />)

    const strategyAside = screen.getByRole('complementary', { name: 'Chat panel' })
    const scrollRegion = within(strategyAside).getByRole('region', { name: 'Chat content' })
    const composerRegion = within(strategyAside).getByRole('region', {
      name: 'Strategy composer region',
    })

    expect(scrollRegion).toContainElement(
      screen.getByRole('heading', { name: market.chatPanel.sectionTitle })
    )
    expect(composerRegion).toContainElement(screen.getByRole('form', { name: 'Strategy composer' }))
    expect(scrollRegion).toHaveAttribute('tabindex', '0')
  })
})

describe('StrategyComposer', () => {
  it('preserves a draft when chat is not connected', async () => {
    const user = userEvent.setup()
    render(<StrategyComposer />)
    const input = screen.getByRole('textbox', { name: 'Strategy instructions' })
    await user.type(input, 'Buy after a timeout{Enter}')
    expect(input).toHaveValue('Buy after a timeout')
    expect(screen.getByRole('button', { name: 'Create strategy' })).toBeDisabled()
  })

  it('controls local text entry and submits the trimmed prompt', async () => {
    const user = userEvent.setup()
    const onSubmit = vi.fn()

    render(<StrategyComposer {...market.chatPanel.composer} onSubmit={onSubmit} />)

    const input = screen.getByRole('textbox', { name: market.chatPanel.composer.inputLabel })
    const submit = screen.getByRole('button', { name: market.chatPanel.composer.submitLabel })

    expect(submit).toBeDisabled()
    await user.type(input, '  Buy LAL after a timeout  ')
    expect(input).toHaveValue('  Buy LAL after a timeout  ')
    expect(submit).toBeEnabled()

    await user.click(submit)

    expect(onSubmit).toHaveBeenCalledWith('Buy LAL after a timeout')
    expect(input).toHaveValue('')
  })

  it('exposes accessible context and tip controls', async () => {
    const user = userEvent.setup()
    const onAddContext = vi.fn()

    render(<StrategyComposer {...market.chatPanel.composer} onAddContext={onAddContext} />)

    await user.click(
      screen.getByRole('button', { name: market.chatPanel.composer.addContextLabel })
    )
    expect(onAddContext).toHaveBeenCalledOnce()
    expect(
      screen.getByRole('link', { name: market.chatPanel.composer.tipLinkLabel })
    ).toHaveAttribute('href', '#signals')

    await user.click(
      screen.getByRole('button', { name: market.chatPanel.composer.dismissTipLabel })
    )
    expect(
      screen.queryByRole('link', { name: market.chatPanel.composer.tipLinkLabel })
    ).not.toBeInTheDocument()
  })
})
