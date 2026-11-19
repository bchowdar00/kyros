import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import {
  BarChartExample,
  CandleChartExample,
  DataTableExample,
  GaugeExample,
  HistogramExample,
  LadderExample,
  ScatterExample,
  StatGridExample,
  TimeSeriesExample,
} from '@/features/chat/examples/data-examples'
import { CardExample, HeatmapExample } from '@/features/chat/examples/panel-examples'
import { Ladder } from '@/features/chat/widgets/ladder'

describe('TimeSeries widget', () => {
  it('names both series in a legend and switches windows', async () => {
    const user = userEvent.setup()
    const { container } = render(<TimeSeriesExample />)
    const legend = within(screen.getByRole('list', { name: 'Series' }))
    expect(legend.getByText('Fair')).toBeVisible()
    expect(legend.getByText('Market')).toBeVisible()

    const windows = within(screen.getByRole('group', { name: 'Window' }))
    const twoMinutes = windows.getByRole('button', { name: '2m' })
    expect(windows.getByRole('button', { name: '4m' })).toHaveAttribute('aria-pressed', 'true')
    await user.click(twoMinutes)
    expect(twoMinutes).toHaveAttribute('aria-pressed', 'true')
    await expectNoA11yViolations(container)
  })
})

describe('CandleChart widget', () => {
  it('shows the last close in the header', async () => {
    const { container } = render(<CandleChartExample />)
    const frame = screen.getByRole('region', { name: 'Bitcoin price candles' })
    const header = within(frame).getByRole('heading', { name: 'BTC, 15s candles' }).parentElement
    expect(header).toHaveTextContent(/\$7\d,\d{3}/)
    await expectNoA11yViolations(container)
  })
})

describe('Histogram widget', () => {
  it('renders the sample count in the header', async () => {
    const { container } = render(<HistogramExample />)
    const frame = screen.getByRole('region', { name: 'Distribution of moves from strike' })
    expect(within(frame).getByText('400 rounds')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('Scatter widget', () => {
  it('names both axes', async () => {
    const { container } = render(<ScatterExample />)
    const frame = screen.getByRole('region', { name: 'Edge at entry against realised profit' })
    expect(within(frame).getByText('Edge at entry →')).toBeVisible()
    expect(within(frame).getByText('↑ P&L')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('Gauge widget', () => {
  it('exposes the value and zone as a meter', async () => {
    const { container } = render(<GaugeExample />)
    const meter = screen.getByRole('meter', { name: 'RSI (14)' })
    expect(meter).toHaveAttribute('aria-valuenow', '27.4')
    expect(meter).toHaveAttribute('aria-valuetext', '27.4, Oversold')
    await expectNoA11yViolations(container)
  })
})

describe('StatGrid widget', () => {
  it('pairs every label with its value in a definition list', async () => {
    const { container } = render(<StatGridExample />)
    const frame = screen.getByRole('region', { name: 'Window statistics' })
    const terms = within(frame).getAllByRole('term')
    expect(terms.map(term => term.textContent)).toContain('Edge')
    expect(within(frame).getAllByRole('definition')).toHaveLength(terms.length)
    expect(within(frame).getByText('+3.4¢')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('DataTable widget', () => {
  it('sorts by a column when its header is activated', async () => {
    const user = userEvent.setup()
    const { container } = render(<DataTableExample />)
    const table = screen.getByRole('table')
    const firstCell = () => within(table).getAllByRole('row')[1]?.textContent ?? ''
    expect(within(table).getByRole('columnheader', { name: /Value/ })).toHaveAttribute(
      'aria-sort',
      'descending'
    )
    expect(firstCell()).toContain('RSI (14)')
    await user.click(within(table).getByRole('button', { name: 'Value' }))
    expect(firstCell()).toContain('ROC')
    await user.click(within(table).getByRole('button', { name: 'Signal' }))
    expect(firstCell()).toContain('Z-score')
    await expectNoA11yViolations(container)
  })
})

const level = (price: number) => ({ price, size: 10 })

describe('Ladder widget', () => {
  it('lists seven asks and seven bids around the spread', async () => {
    const { container } = render(<LadderExample />)
    const asks = screen.getByRole('list', { name: 'Asks, best price last' })
    const bids = screen.getByRole('list', { name: 'Bids, best price first' })
    expect(within(asks).getAllByRole('listitem')).toHaveLength(7)
    expect(within(bids).getAllByRole('listitem')).toHaveLength(7)
    expect(screen.getByText('Spread 1¢')).toBeVisible()
    await expectNoA11yViolations(container)
  })

  it('shows a deeper book in keyboard-reachable scroll regions around a fixed spread', async () => {
    const { container } = render(
      <Ladder
        label="Deep YES book"
        asks={Array.from({ length: 15 }, (_, index) => level(63 + index))}
        bids={Array.from({ length: 15 }, (_, index) => level(61 - index))}
        rows={15}
        formatPrice={price => `${price}¢`}
        formatSize={size => String(size)}
        midLabel="Spread 2¢"
      />
    )
    const asks = screen.getByRole('list', { name: 'Asks, best price last' })
    expect(within(asks).getAllByRole('listitem')).toHaveLength(15)
    expect(asks).toHaveAttribute('tabindex', '0')
    expect(screen.getByRole('list', { name: 'Bids, best price first' })).toHaveAttribute(
      'tabindex',
      '0'
    )
    await expectNoA11yViolations(container)
  })
})

describe('Card widget', () => {
  it('leads with the hero value and lists its fields', async () => {
    const { container } = render(<CardExample />)
    const frame = screen.getByRole('region', { name: 'Mean reversion strategy summary' })
    expect(within(frame).getByText('+$42.10')).toBeVisible()
    expect(within(frame).getAllByRole('term')).toHaveLength(6)
    await expectNoA11yViolations(container)
  })
})

describe('Heatmap widget', () => {
  it('is a table with a row header per regime', async () => {
    const { container } = render(<HeatmapExample />)
    const table = within(
      screen.getByRole('region', { name: 'Average edge by regime and time remaining' })
    ).getByRole('table')
    expect(within(table).getByRole('rowheader', { name: 'Trend' })).toBeVisible()
    expect(within(table).getAllByRole('cell')).toHaveLength(12)
    await expectNoA11yViolations(container)
  })
})

describe('BarChart widget', () => {
  it('renders a named region with its period in the header', async () => {
    const { container } = render(<BarChartExample />)
    const frame = screen.getByRole('region', { name: 'Profit and loss per round' })
    expect(within(frame).getByText('Last 12 rounds')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})
