import { render, screen, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import type { RecordBoardView } from '@/features/records/record-board-loader'
import { RecordBoardScreen } from '@/features/records/record-board-screen'

const EXPLORER = 'https://explorer.test'
const A = `0x1e6b${'0'.repeat(56)}42ca`
const B = `0x${'b'.repeat(64)}`
const TX_A = `0x${'aa'.repeat(32)}`
const TX_B = `0x${'bb'.repeat(32)}`

const board: RecordBoardView = {
  asOf: '2026-11-20T08:30:00.000Z',
  contract: '0xc910cfe6b537feee16b60a9964c55fc66580586a',
  rows: [
    {
      budgetCents: 4000,
      deploymentId: A,
      lastRoundId: 231,
      lastSealedAt: '2026-11-20T08:25:23.000000Z',
      lastTransactionHash: TX_A,
      name: 'Quote With the Lean',
      openTransactionHash: `0x${'cc'.repeat(32)}`,
      pnlCents: 1250,
      rounds: 12,
      series: 'btc-5m',
      wonRounds: 7,
    },
    {
      budgetCents: undefined,
      deploymentId: B,
      lastRoundId: 229,
      lastSealedAt: '2026-11-20T08:20:15.000000Z',
      lastTransactionHash: TX_B,
      name: undefined,
      openTransactionHash: undefined,
      pnlCents: -996,
      rounds: 1,
      series: undefined,
      wonRounds: 0,
    },
  ],
  status: 'ok',
  totals: { rounds: 13, strategies: 2, wonRounds: 7 },
  truncated: false,
}

describe('RecordBoardScreen', () => {
  it('lists the strategies in the order given, with what each sealed', async () => {
    const { container } = render(<RecordBoardScreen explorerUrl={EXPLORER} view={board} />)

    expect(screen.getByRole('heading', { level: 1, name: 'Records' })).toBeInTheDocument()
    expect(screen.getByText('2 strategies · 13 rounds sealed · 7 won')).toBeInTheDocument()

    const [, first, second] = screen.getAllByRole('row')
    if (first === undefined || second === undefined) throw new Error('expected two rows')

    expect(within(first).getByRole('rowheader')).toHaveTextContent('Quote With the Lean')
    expect(within(first).getByRole('rowheader')).toHaveTextContent('BTC 5m · $40.00 budget')
    expect(within(first).getByText('12')).toBeInTheDocument()
    expect(within(first).getByText('7')).toBeInTheDocument()
    expect(within(first).getByText('+$12.50')).toBeInTheDocument()
    expect(within(first).getByText('Nov 20, 08:25 UTC')).toBeInTheDocument()

    expect(within(second).getByRole('rowheader')).toHaveTextContent('Strategy 0xbbbb…bbbb')
    expect(within(second).getByText('-$9.96')).toBeInTheDocument()

    await expectNoA11yViolations(container)
  })

  it('links each strategy to its newest sealed round on the explorer, and the contract too', () => {
    render(<RecordBoardScreen explorerUrl={EXPLORER} view={board} />)

    expect(
      screen.getByRole('link', {
        name: 'View on Arc: the newest sealed round of Quote With the Lean',
      })
    ).toHaveAttribute('href', `${EXPLORER}/tx/${TX_A}`)
    expect(
      screen.getByRole('link', {
        name: 'View on Arc: the newest sealed round of Strategy 0xbbbb…bbbb',
      })
    ).toHaveAttribute('href', `${EXPLORER}/tx/${TX_B}`)
    expect(screen.getByRole('link', { name: /0xc910…586a/ })).toHaveAttribute(
      'href',
      `${EXPLORER}/address/0xc910cfe6b537feee16b60a9964c55fc66580586a`
    )
  })

  it('says where the numbers come from and that the money is pretend', () => {
    render(<RecordBoardScreen explorerUrl={EXPLORER} view={board} />)

    expect(
      screen.getByText(/added up from those transactions, not from our server/)
    ).toBeInTheDocument()
    expect(
      screen.getByText(/the server supplies only a strategy's name and market/)
    ).toBeInTheDocument()
    expect(screen.getByText(/pretend money/)).toBeInTheDocument()
    expect(screen.getByText(/Read from Arc Testnet at Nov 20, 08:30 UTC/)).toBeInTheDocument()
  })

  it('says so when no round has been sealed', () => {
    render(
      <RecordBoardScreen
        explorerUrl={EXPLORER}
        view={{ ...board, rows: [], totals: { rounds: 0, strategies: 0, wonRounds: 0 } }}
      />
    )

    expect(screen.getByText('No round has been sealed yet.')).toBeInTheDocument()
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
  })

  it('says so when older rounds were not read', () => {
    render(<RecordBoardScreen explorerUrl={EXPLORER} view={{ ...board, truncated: true }} />)

    expect(screen.getByText(/older rounds are not counted/)).toBeInTheDocument()
  })

  it('says the explorer is not answering instead of showing an empty board', () => {
    render(<RecordBoardScreen explorerUrl={EXPLORER} view={{ status: 'unavailable' }} />)

    expect(screen.getByRole('status')).toHaveTextContent(
      'The Arc explorer is not answering right now, so this page cannot be built. Reload in a minute.'
    )
    expect(screen.queryByRole('table')).not.toBeInTheDocument()
  })

  it('shows a round sealed at nothing as $0.00, neither a gain nor a loss', () => {
    const [row] = board.status === 'ok' ? board.rows : []
    if (row === undefined || board.status !== 'ok') throw new Error('expected a row')
    render(
      <RecordBoardScreen
        explorerUrl={EXPLORER}
        view={{ ...board, rows: [{ ...row, pnlCents: 0, wonRounds: 0 }] }}
      />
    )

    const cell = screen.getByRole('cell', { name: '$0.00' })
    expect(cell).not.toHaveClass('text-positive')
    expect(cell).not.toHaveClass('text-negative')
  })

  it('still shows a row whose block time cannot be read', () => {
    const [row] = board.status === 'ok' ? board.rows : []
    if (row === undefined || board.status !== 'ok') throw new Error('expected a row')
    render(
      <RecordBoardScreen
        explorerUrl={EXPLORER}
        view={{ ...board, rows: [{ ...row, lastSealedAt: 'not a time' }] }}
      />
    )

    expect(screen.getByRole('rowheader')).toHaveTextContent('Quote With the Lean')
    expect(screen.getByText('Time not given')).toBeInTheDocument()
  })
})
