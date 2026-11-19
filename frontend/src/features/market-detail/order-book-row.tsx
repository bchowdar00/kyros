import { formatExchangeDollars, formatExchangeTicks } from '@kyros/money'
import { cn } from '@kyros/ui/lib/cn'

import type { MarketSide, OrderBookLevel } from './types'

export interface OrderBookRowProps {
  depthPercentage: number
  level: OrderBookLevel
  side: MarketSide
}

/* Offers sit on the grey wash and bids on the blue one; the bid price is the blue number. */
const depthClasses: Record<MarketSide, string> = {
  away: 'bg-surface-raised',
  home: 'bg-action-muted',
}

const priceClasses: Record<MarketSide, string> = {
  away: 'text-foreground',
  home: 'text-positive',
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
    <tr className="table h-3xl w-full table-fixed" data-order-book-side={side}>
      <td className="relative w-1/3 overflow-visible pl-sm text-left">
        <span
          aria-hidden="true"
          className={cn('absolute inset-y-px left-0 z-0 rounded-xs', depthClasses[side])}
          data-order-book-depth=""
          style={{ width: `${depthPercentage * 3}%` }}
        />
        <span className={cn('relative z-[1] font-bold', priceClasses[side])}>
          {'priceTicks' in level ? formatExchangeTicks(level.priceTicks) : `${level.priceCents}¢`}
        </span>
      </td>
      <td className="relative z-[1] w-1/3 text-center">
        {'priceTicks' in level ? level.qty : sharesFormatter.format(level.shares)}
      </td>
      <td className="relative z-[1] w-1/3 pr-sm text-right">
        {'priceTicks' in level
          ? formatExchangeDollars(level.totalMicros)
          : usdFormatter.format(level.totalUsd)}
      </td>
    </tr>
  )
}
