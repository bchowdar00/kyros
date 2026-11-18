import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'
import { ChatPanel } from '@/features/market-detail/chat-panel'

const btcRail = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].chatPanel
const nbaRail = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].chatPanel

describe('ChatPanel chat view', () => {
  it('does not present a fixture transcript as a real conversation', async () => {
    const { container } = render(<ChatPanel data={btcRail} />)
    expect(screen.queryByText('What is BTC volume like?')).not.toBeInTheDocument()
    expect(screen.queryByText(/trailing 24 hours/)).not.toBeInTheDocument()
    expect(screen.getByRole('heading', { name: btcRail.sectionTitle })).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: 'Message Teeny' })).toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('keeps the strategies view for markets that are not chatting', () => {
    render(<ChatPanel data={nbaRail} />)

    expect(screen.getByRole('heading', { name: nbaRail.sectionTitle })).toBeInTheDocument()
    expect(screen.queryByText('What is BTC volume like?')).not.toBeInTheDocument()
  })
})
