import { formatExchangeTicks, formatExchangeBalance } from '@tiny/money'
import { cn } from '@tiny/ui/lib/cn'

import type { MarketSide, OrderBookLevel } from './types'

export interface OrderBookRowProps {
  depthPercentage: number
  level: OrderBookLevel
  side: MarketSide
}

const depthClasses: Record<MarketSide, string> = {
  away: 'border-negative/10 bg-negative/25',
  home: 'border-positive/10 bg-positive/25',
}

const sharesFormatter = new Intl.NumberFormat('en-US', {
  maximumFractionDigits: 2,
  minimumFractionDigits: 2,
})

const usdFormatter = new Intl.NumberFormat('en-US', {
  currency: 'USD',
  maximumFractionDigits: 2,
  minimumFractionDigits: 2,
  style: 'currency',
})

export function OrderBookRow({ depthPercentage, level, side }: OrderBookRowProps) {
  return (
    <tr className="mb-xs table h-[28px] w-full table-fixed" data-order-book-side={side}>
      <td className="relative w-1/3 overflow-visible pl-sm text-left">
        <span
          aria-hidden="true"
          className={cn('absolute inset-y-0 left-0 z-0 rounded-xs border', depthClasses[side])}
          data-order-book-depth=""
          style={{ width: `${depthPercentage * 3}%` }}
        />
        <span className="relative z-[1]">
          {'priceTicks' in level ? formatExchangeTicks(level.priceTicks) : `${level.priceCents}¢`}
        </span>
      </td>
      <td className="relative z-[1] w-1/3 text-center">
        {'priceTicks' in level ? level.qty : sharesFormatter.format(level.shares)}
      </td>
      <td className="relative z-[1] w-1/3 pr-sm text-right">
        {'priceTicks' in level
          ? formatExchangeBalance(level.totalMicros)
          : usdFormatter.format(level.totalUsd)}
      </td>
    </tr>
  )
}
