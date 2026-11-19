import { formatExchangeTicks } from '@kyros/money'
import type { TimeRange } from '@kyros/ui/charts/lib/types'
import { cn } from '@kyros/ui/lib/cn'
import Image from 'next/image'

import { MarketBuyButton } from './market-buy-button'
import { MarketHeaderMotion } from './market-header-motion'
import { MarketHeaderSubtitle } from './market-header-subtitle'
import { MarketPayoutPreview } from './market-payout-preview'
import type { MarketHeaderData, MarketOutcome, MarketSide } from './types'

export type MarketHeaderProps = MarketHeaderData & {
  marketSlug: string
  marketKey?: string
  range?: TimeRange | undefined
  tradingUnavailableReason?: string | undefined
}

const outcomeClasses: Record<MarketSide, string> = {
  away: 'border-market-away bg-market-away/80 hover:bg-market-away',
  home: 'border-market-home bg-market-home/80 hover:bg-market-home',
}

/* Up is the blue side and Down the ink side, everywhere: outlined at rest, filled on hover. */
const upClasses =
  'border-action text-positive hover:border-action hover:bg-action hover:text-on-action'
const downClasses =
  'border-foreground text-foreground hover:border-foreground hover:bg-foreground hover:text-background'

const binaryOutcomeClasses = {
  DOWN: downClasses,
  NO: downClasses,
  UP: upClasses,
  YES: upClasses,
} as const

const binaryOutcomeNames = { DOWN: 'Down', NO: 'No', UP: 'Up', YES: 'Yes' } as const

function isBinaryOutcomeLabel(label: string): label is keyof typeof binaryOutcomeClasses {
  return label === 'DOWN' || label === 'NO' || label === 'UP' || label === 'YES'
}

function getOutcomeClass({ label, side }: MarketOutcome, marketSlug: string) {
  if (!marketSlug.includes('/sports') && isBinaryOutcomeLabel(label)) {
    return binaryOutcomeClasses[label]
  }

  return outcomeClasses[side]
}

export function MarketHeader({
  marketSlug,
  marketKey = marketSlug,
  outcomes,
  range,
  teams,
  title,
  tradingUnavailableReason,
}: MarketHeaderProps) {
  return (
    <MarketHeaderMotion
      actions={
        <div className="grid auto-cols-fr grid-flow-col gap-sm">
          {outcomes.map(outcome => {
            const price =
              outcome.priceTicks === undefined
                ? outcome.priceCents === undefined
                  ? undefined
                  : `${outcome.priceCents}¢`
                : formatExchangeTicks(outcome.priceTicks)
            const reason =
              tradingUnavailableReason ?? (price === undefined ? 'Price unavailable' : undefined)
            return (
              <MarketBuyButton
                marketKey={marketKey}
                disabled={reason !== undefined}
                side={outcome.side}
                aria-label={`Buy ${outcome.label} outcome${price === undefined ? '' : ` at ${price.replace('¢', ' cents')}`}`}
                aria-description={reason}
                title={reason}
                className={cn(
                  'h-auto min-w-32 flex-col items-start gap-0 rounded-md border-2 px-lg pb-md pt-sm text-left',
                  getOutcomeClass(outcome, marketSlug)
                )}
                key={outcome.side}
                size="large"
                variant="secondary"
              >
                <span className="font-display text-heading-sm font-bold">
                  Buy{' '}
                  {isBinaryOutcomeLabel(outcome.label)
                    ? binaryOutcomeNames[outcome.label]
                    : outcome.label}
                </span>
                <span className="font-display text-price font-extrabold tracking-display">
                  {price === undefined ? '—' : price.replace('¢', '')}
                  {price === undefined ? null : (
                    <span className="ml-px align-top text-heading-md font-bold tracking-normal">
                      ¢
                    </span>
                  )}
                </span>
              </MarketBuyButton>
            )
          })}
        </div>
      }
      actionSubtitle={
        tradingUnavailableReason ? (
          <p className="max-w-prose text-right text-label text-muted-foreground">
            {tradingUnavailableReason}
          </p>
        ) : (
          <div className="grid auto-cols-fr grid-flow-col gap-sm">
            {outcomes.map(outcome =>
              outcome.priceCents === undefined && outcome.priceTicks === undefined ? (
                <span key={outcome.side} className="text-center text-label text-muted-foreground">
                  Price unavailable
                </span>
              ) : (
                <MarketPayoutPreview
                  key={outcome.side}
                  outcomeLabel={outcome.label}
                  priceCents={outcome.priceCents}
                  priceTicks={outcome.priceTicks}
                />
              )
            )}
          </div>
        )
      }
      identity={
        <div className="flex min-w-0 items-start gap-md">
          {teams === undefined ? null : (
            // The two sides' marks, away then home, overlapping like the home card's.
            <div className="flex shrink-0 items-center" data-slot="market-header-teams">
              {teams.map((team, index) => (
                <div
                  className={cn({
                    '-ml-xl': index > 0,
                    'flex size-avatar-lg items-center justify-center rounded-full border border-highlight bg-background': true,
                  })}
                  key={team.id}
                >
                  <Image
                    alt=""
                    className={team.logoInset ? 'h-xl w-2xl' : 'size-avatar-md'}
                    draggable={false}
                    height={team.logoInset ? 20 : 36}
                    src={team.logo.src}
                    unoptimized
                    width={team.logoInset ? 24 : 36}
                  />
                </div>
              ))}
            </div>
          )}
          <h1 className="min-w-0 truncate font-display text-display-md font-bold tracking-display text-foreground">
            {title}
          </h1>
        </div>
      }
      subtitle={range ? <MarketHeaderSubtitle marketSlug={marketSlug} range={range} /> : null}
    />
  )
}
