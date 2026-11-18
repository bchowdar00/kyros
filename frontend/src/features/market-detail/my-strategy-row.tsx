import { createMoneyFormatter } from '@tiny/money'
import { Badge } from '@tiny/ui/components/badge'
import { MiniForwardIcon } from '@tiny/ui/icons/mini-forward-icon'
import { cn } from '@tiny/ui/lib/cn'

import type { MarketSide, MyStrategy } from './types'

export interface MyStrategyRowProps {
  showDivider?: boolean
  strategy: MyStrategy
}

const positionDotClasses: Record<MarketSide, string> = {
  away: 'border-market-away bg-market-away/80',
  home: 'border-market-home bg-market-home/80',
}

const money = createMoneyFormatter('en-US')

export function MyStrategyRow({ showDivider = false, strategy }: MyStrategyRowProps) {
  const pnlIsPositive = strategy.totalPnlCents >= 0

  return (
    <tr className={cn('h-[76px]', showDivider && 'border-b border-border-subtle')}>
      <th
        className="max-w-[300px] overflow-hidden py-xl pl-xl text-left font-regular align-middle"
        scope="row"
      >
        <div className="flex min-w-0 items-center gap-sm">
          <span
            className="block min-w-0 flex-1 truncate text-body font-regular text-foreground"
            title={strategy.name}
          >
            {strategy.name}
          </span>
          {strategy.status === 'paused' ? (
            <Badge
              className="h-[14px] shrink-0 border-0 bg-highlight px-[6px] py-0 text-caption font-semibold uppercase"
              variant="neutral"
            >
              Paused
            </Badge>
          ) : null}
        </div>
      </th>

      <td className="overflow-hidden whitespace-nowrap pl-sm align-middle text-label font-medium">
        {strategy.position ? (
          <span className="flex min-w-0 flex-col gap-xs">
            <span className="flex min-w-0 items-center gap-sm text-foreground">
              <span
                aria-hidden="true"
                className={cn(
                  'size-sm shrink-0 rounded-pill border-[0.5px]',
                  positionDotClasses[strategy.position.side]
                )}
              />
              <span className="truncate font-mono">{strategy.position.label}</span>
            </span>
            <span className="font-mono text-muted-foreground">
              {money.usd(strategy.position.valueCents)}
            </span>
          </span>
        ) : (
          <span className="flex items-center gap-sm text-muted-foreground">
            <span aria-hidden="true" className="h-px w-sm shrink-0 bg-muted-foreground" />
            No Position
          </span>
        )}
      </td>

      <td className="overflow-hidden whitespace-nowrap text-center align-middle font-mono text-body font-medium text-foreground">
        {strategy.trades}
      </td>
      <td
        className={cn(
          'overflow-hidden whitespace-nowrap pl-sm text-right align-middle font-mono text-body font-medium',
          pnlIsPositive ? 'text-positive' : 'text-negative'
        )}
      >
        {money.usd(strategy.totalPnlCents, { signDisplay: 'always' })}
      </td>
      <td className="overflow-hidden pr-xl text-right align-middle">
        <MiniForwardIcon className="ml-auto size-[18px] text-muted-foreground" />
      </td>
    </tr>
  )
}
