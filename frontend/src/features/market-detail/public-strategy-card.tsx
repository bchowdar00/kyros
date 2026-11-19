'use client'

import { createMoneyFormatter } from '@kyros/money'
import { Button } from '@kyros/ui/components/button'
import { useId, type ComponentPropsWithRef } from 'react'
import { cn } from '@kyros/ui/lib/cn'

import type { PublicStrategy } from './types'
import { useTradeTicket } from './use-trade-ticket'

export type PublicStrategyAction = (strategy: PublicStrategy) => void

export interface PublicStrategyCardProps extends ComponentPropsWithRef<'article'> {
  marketKey?: string
  onRun?: PublicStrategyAction
  onInverse?: PublicStrategyAction
  strategy: PublicStrategy
}

const moneyFormatter = createMoneyFormatter('en-US')

export function PublicStrategyCard({
  marketKey = 'standalone-chat',
  onRun,
  onInverse,
  strategy,
  className,
  ...props
}: PublicStrategyCardProps) {
  const headingId = `public-strategy-${strategy.id}`
  const triggerId = useId()
  const { ticket } = useTradeTicket(marketKey)

  return (
    <article
      {...props}
      aria-labelledby={headingId}
      className={cn('border-b border-border-subtle py-xl first:pt-0', className)}
      data-public-strategy-id={strategy.id}
    >
      <div className="flex flex-col gap-md">
        <div className="min-w-0">
          <h3
            className="min-w-0 truncate font-display text-heading-md font-extrabold tracking-heading text-foreground"
            id={headingId}
          >
            {strategy.title}
          </h3>

          {/* What Run does, in the template's own words. */}
          <p className="mt-xs line-clamp-2 text-body text-foreground">{strategy.logic}</p>

          <p className="mt-xs truncate text-label text-muted-foreground">
            A template by {strategy.creator.handle}
          </p>
        </div>

        <div className="flex min-w-0 flex-wrap items-center justify-between gap-md">
          <div className="flex min-w-0 items-center gap-md">
            <p className="shrink-0 text-body font-medium text-foreground">
              <span className="font-mono">{moneyFormatter.count(strategy.runningCount)}</span>{' '}
              <span className="text-label text-muted-foreground">running</span>
            </p>

            <p className="shrink-0 text-body font-medium text-foreground">
              <span className="font-mono">
                {moneyFormatter.usd(strategy.deployedCents, {
                  compact: true,
                  maximumFractionDigits: 2,
                })}
              </span>{' '}
              <span className="text-label text-muted-foreground">deployed</span>
            </p>
          </div>

          <div className="flex shrink-0 items-center gap-xs">
            <Button
              aria-label={`Inverse ${strategy.title}`}
              className="relative px-md after:absolute after:inset-x-0 after:-inset-y-sm after:content-['']"
              id={`${triggerId}-inverse`}
              onClick={() =>
                onInverse
                  ? onInverse(strategy)
                  : ticket.openStrategy(strategy, true, `${triggerId}-inverse`)
              }
              size="small"
              variant="secondary"
            >
              Inverse
            </Button>
            <Button
              aria-label={`Run ${strategy.title}`}
              className="relative w-strategy-action after:absolute after:inset-x-0 after:-inset-y-sm after:content-['']"
              id={`${triggerId}-run`}
              onClick={() =>
                onRun ? onRun(strategy) : ticket.openStrategy(strategy, false, `${triggerId}-run`)
              }
              size="small"
            >
              Run
            </Button>
          </div>
        </div>
      </div>
    </article>
  )
}
