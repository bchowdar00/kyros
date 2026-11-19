import { expectNoA11yViolations } from '@kyros/testing/a11y'
import type { RustGeminiSeries } from '@kyros/domain'
import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import { GeminiBookSource } from './gemini-book-source'

const open: RustGeminiSeries = {
  env: 'production',
  series: 'BTC05M',
  phase: 'open',
  book: 'live',
  event_ticker: 'BTC05M2611182100',
  symbol: 'GEMI-BTC05M2611182100-UP',
}

afterEach(cleanup)

describe('gemini book source', () => {
  it.each([
    ['live', 'Live'],
    ['stale', 'Stale'],
    ['waiting', 'Waiting'],
  ] as const)('names a %s book', (book, label) => {
    render(<GeminiBookSource status="polled" gemini={{ ...open, book }} />)
    expect(screen.getByText('Book from Gemini')).toBeInTheDocument()
    expect(screen.getByText(label)).toBeInTheDocument()
  })

  it('says the book state is unknown when the last poll failed', () => {
    render(<GeminiBookSource status="poll-failed" gemini={open} />)
    expect(screen.getByText('Book from Gemini')).toBeInTheDocument()
    expect(screen.getByText('Unknown')).toBeInTheDocument()
    expect(screen.queryByText('Live')).not.toBeInTheDocument()
  })

  it('hides the last contract after a failed poll', () => {
    render(<GeminiBookSource status="poll-failed" gemini={open} />)
    expect(
      screen.queryByRole('link', { name: 'View GEMI-BTC05M2611182100-UP on Gemini' })
    ).not.toBeInTheDocument()
    expect(screen.queryByText('GEMI-BTC05M2611182100-UP')).not.toBeInTheDocument()
  })

  it('does not name Gemini when a poll failed before any block was known', () => {
    render(<GeminiBookSource status="poll-failed" gemini={undefined} />)
    expect(screen.getByText('Book source unknown')).toBeInTheDocument()
    expect(screen.queryByText(/Gemini/)).not.toBeInTheDocument()
    expect(screen.queryByText('Unknown')).not.toBeInTheDocument()
  })

  it("links a production contract to Gemini's page", async () => {
    const { container } = render(<GeminiBookSource status="polled" gemini={open} />)
    const link = screen.getByRole('link', { name: 'View GEMI-BTC05M2611182100-UP on Gemini' })
    expect(link).toHaveAttribute('href', 'https://www.gemini.com/predictions/BTC05M2611182100')
    expect(link).toHaveAttribute('target', '_blank')
    expect(link).toHaveAttribute('rel', 'noopener noreferrer')
    await expectNoA11yViolations(container)
  })

  it('shows a sandbox contract without a link', () => {
    render(<GeminiBookSource status="polled" gemini={{ ...open, env: 'sandbox' }} />)
    expect(screen.getByText('Book from Gemini sandbox')).toBeInTheDocument()
    expect(screen.getByText('GEMI-BTC05M2611182100-UP')).toBeInTheDocument()
    expect(screen.queryByRole('link')).not.toBeInTheDocument()
  })

  it('shows no contract between rounds', () => {
    const { event_ticker: _ticker, symbol: _symbol, ...between } = open
    render(
      <GeminiBookSource status="polled" gemini={{ ...between, phase: 'waiting_for_window' }} />
    )
    expect(screen.queryByRole('link')).not.toBeInTheDocument()
    expect(screen.queryByText(/GEMI-/)).not.toBeInTheDocument()
  })

  it('renders nothing for a series Gemini does not back', () => {
    const { container } = render(<GeminiBookSource status="polled" gemini={undefined} />)
    expect(container).toBeEmptyDOMElement()
  })
})
