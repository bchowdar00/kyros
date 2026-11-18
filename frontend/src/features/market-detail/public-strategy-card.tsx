'use client'

import { createMoneyFormatter } from '@tiny/money'
import { Avatar } from '@tiny/ui/components/avatar'
import { Button } from '@tiny/ui/components/button'
import Image from 'next/image'
import { useId, type ComponentPropsWithRef } from 'react'
import { cn } from '@tiny/ui/lib/cn'

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
      className={cn(
        'relative overflow-clip rounded-md border-[0.5px] border-border-subtle/50 bg-surface-subtle p-xl',
        className
      )}
      data-public-strategy-id={strategy.id}
    >
      {strategy.creator.avatar ? (
        <div
          aria-hidden="true"
          className="pointer-events-none absolute top-1/2 left-0 size-strategy-glow -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-pill opacity-5 blur-strategy-glow"
        >
          <Image
            alt=""
            className="object-cover"
            fill
            sizes="144px"
            src={strategy.creator.avatar.src}
          />
        </div>
      ) : null}

      <div className="relative flex flex-col gap-xl">
        <div className="flex items-center gap-md">
          <Avatar
            alt={strategy.creator.avatar?.alt ?? `${strategy.creator.handle} avatar`}
            className="size-avatar-lg bg-background"
            fallback={strategy.creator.handle.slice(1, 3).toUpperCase()}
            framed
            {...(strategy.creator.avatar === undefined ? {} : { src: strategy.creator.avatar.src })}
          />

          <div className="min-w-0 flex-1">
            <h3
              className="min-w-0 truncate text-heading-sm font-regular leading-none text-foreground"
              id={headingId}
            >
              {strategy.title}
            </h3>

            <p className="mt-copy truncate text-label font-medium text-muted-foreground">
              By {strategy.creator.handle}
            </p>
          </div>
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
              className="relative border-[0.5px] border-border-subtle/50 bg-surface-subtle px-md hover:bg-surface after:absolute after:inset-x-0 after:-inset-y-sm after:content-['']"
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
