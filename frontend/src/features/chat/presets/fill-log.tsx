import { createMoneyFormatter } from '@kyros/money'

import { DataTable } from '../widgets/data-table'

export interface Fill {
  id: string
  side: 'YES' | 'NO'
  /** Buy or sell of that side. */
  action: 'buy' | 'sell'
  priceCents: number
  size: number
  /** Already formatted for display. */
  time: string
}

export interface FillLogProps {
  series: string
  /** Newest first. */
  fills: readonly Fill[]
}

const money = createMoneyFormatter('en-US')

const COLUMNS = [
  { id: 'time', label: 'Time' },
  { id: 'side', label: 'Fill' },
  { id: 'price', label: 'Price', align: 'right' as const, sortable: true },
  { id: 'size', label: 'Size', align: 'right' as const, sortable: true },
]

/** Recent fills for the account in one series, as a dense table preset. */
export function FillLog({ series, fills }: FillLogProps) {
  return (
    <DataTable
      label={`${series} fill log`}
      title="Fills"
      meta={`${fills.length} in this round`}
      columns={COLUMNS}
      dense
      rows={fills.map(fill => ({
        id: fill.id,
        cells: {
          time: { kind: 'text', text: fill.time, muted: true },
          side: {
            kind: 'pip',
            state: fill.action === 'buy' ? 'on' : 'off',
            text: `${fill.action === 'buy' ? 'Buy' : 'Sell'} ${fill.side}`,
          },
          price: { kind: 'number', text: `${fill.priceCents}¢`, value: fill.priceCents },
          size: { kind: 'number', text: money.count(fill.size), value: fill.size },
        },
      }))}
    />
  )
}
