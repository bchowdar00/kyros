import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import type { RustSignalCatalogue } from '@tiny/domain'
import { describe, expect, it, vi } from 'vitest'

import { SignalsBrowser } from '@/features/market-detail/signals-browser'

const catalogue: RustSignalCatalogue = {
  series: 'btc-2m',
  underlying: 'btc',
  signals: [
    {
      signal_id: 'btc-2m.current.stats.v1',
      name: 'Round activity',
      description:
        'How much has traded in this round so far. Everything here resets when the next round opens.',
      location: 'internal',
      scope: { kind: 'series', series_id: 'btc-2m' },
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [
        {
          name: 'trades',
          label: 'Trades',
          type: 'u32',
          headline: true,
          description:
            'How many fills have happened this round. A fill that crosses several orders counts once per order.',
          value: 12,
        },
        { name: 'traders', label: 'Traders', type: 'u32', headline: true },
      ],
    },
    {
      signal_id: 'btc.coinbase.rsi.v1',
      name: 'RSI',
      description:
        'Relative strength over the last fourteen one-minute bars, from zero to one hundred.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'spot',
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [
        { name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true, value: '61.2' },
        { name: 'bar_count', label: 'Bar Count', type: 'u16', headline: false, value: 14 },
      ],
    },
    {
      signal_id: 'btc.kraken.rsi.v1',
      name: 'RSI',
      description:
        'Relative strength over the last fourteen one-minute bars, from zero to one hundred.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'spot',
      live: true,
      quality: { state: 'warming_up', reasons: [] },
      fields: [{ name: 'rsi_14', label: 'RSI', type: 'decimal', headline: true, value: '50' }],
    },
  ],
}

function renderBrowser(overrides: Partial<Parameters<typeof SignalsBrowser>[0]> = {}) {
  return render(
    <SignalsBrowser
      catalogue={catalogue}
      history={{}}
      onClose={() => undefined}
      onTogglePin={() => undefined}
      pins={[]}
      {...overrides}
    />
  )
}

describe('SignalsBrowser', () => {
  it('opens the market by default, keeps other venues closed, and hides warming-up signals', async () => {
    const { container } = renderBrowser({ pins: ['btc-2m.current.stats.v1:trades'] })

    expect(
      screen.getByText('2 live values for btc-2m. Pinned rows show on the strip.')
    ).toBeInTheDocument()
    expect(screen.queryByText('Kraken')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /Traders/ })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /Bar Count/ })).not.toBeInTheDocument()

    expect(screen.getByRole('heading', { name: 'Round activity' })).toBeVisible()
    expect(screen.getByRole('button', { name: 'Unpin Round activity Trades' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )
    expect(screen.getByRole('heading', { name: 'RSI' })).not.toBeVisible()

    await expectNoA11yViolations(container)
  })

  it('expands a venue on click and pins from its rows', async () => {
    const user = userEvent.setup()
    const onTogglePin = vi.fn()
    renderBrowser({ onTogglePin })

    await user.click(screen.getByText('Coinbase'))
    expect(screen.getByRole('heading', { name: 'RSI' })).toBeVisible()
    expect(
      screen.getByText(
        'Relative strength over the last fourteen one-minute bars, from zero to one hundred.'
      )
    ).toBeVisible()

    await user.click(screen.getByRole('button', { name: 'Pin RSI RSI' }))
    expect(onTogglePin).toHaveBeenCalledWith('btc.coinbase.rsi.v1:rsi_14')
  })

  it('reveals the hidden plumbing fields with Show all fields', async () => {
    const user = userEvent.setup()
    renderBrowser()

    const toggle = screen.getByRole('button', { name: 'Show all fields' })
    expect(toggle).toHaveAttribute('aria-pressed', 'false')
    await user.click(toggle)
    expect(toggle).toHaveAttribute('aria-pressed', 'true')
    expect(screen.getByRole('button', { name: 'Pin RSI Bar Count' })).toBeInTheDocument()
    expect(screen.getByText(/^3 live values/)).toBeInTheDocument()
  })

  it('opens a row into its detail view, and Escape returns to the row before closing', async () => {
    const user = userEvent.setup()
    const onClose = vi.fn()
    const onTogglePin = vi.fn()
    renderBrowser({ onClose, onTogglePin })

    const row = screen.getByRole('button', { name: /^Trades/ })
    await user.click(row)
    const detail = screen.getByRole('dialog', { name: 'Trades' })
    expect(detail).toHaveTextContent('Round activity on This market')
    expect(detail).toHaveTextContent('How many fills have happened this round.')
    expect(screen.getByRole('button', { name: 'Close Trades' })).toHaveFocus()

    await user.click(screen.getByRole('button', { name: 'Pin to strip' }))
    expect(onTogglePin).toHaveBeenCalledWith('btc-2m.current.stats.v1:trades')

    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog', { name: 'Trades' })).not.toBeInTheDocument()
    expect(row).toHaveFocus()
    expect(onClose).not.toHaveBeenCalled()

    await user.keyboard('{Escape}')
    expect(onClose).toHaveBeenCalled()
  })

  it('searches across every venue and closes on Escape', async () => {
    const user = userEvent.setup()
    const onClose = vi.fn()
    renderBrowser({ onClose })

    await user.type(screen.getByRole('searchbox', { name: 'Search signals' }), 'rsi')
    expect(screen.queryByRole('button', { name: /Round activity/ })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Pin RSI RSI' })).toBeVisible()

    await user.type(screen.getByRole('searchbox', { name: 'Search signals' }), ' nothing')
    expect(screen.getByText(/No live values match/)).toBeInTheDocument()

    await user.keyboard('{Escape}')
    expect(onClose).toHaveBeenCalled()
  })
})
