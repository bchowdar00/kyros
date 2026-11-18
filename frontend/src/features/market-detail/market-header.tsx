import { formatExchangeTicks } from '@tiny/money'
import type { TimeRange } from '@tiny/ui/charts/lib/types'
import { NanoDotIcon } from '@tiny/ui/icons/nano-dot-icon'
import { cn } from '@tiny/ui/lib/cn'
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

const binaryOutcomeClasses = {
  DOWN: 'border-negative bg-negative/80 hover:bg-negative/80',
  NO: 'border-negative bg-negative/80 hover:bg-negative/80',
  UP: 'border-positive bg-positive/80 text-foreground hover:bg-positive hover:text-foreground',
  YES: 'border-positive bg-positive/80 text-foreground hover:bg-positive hover:text-foreground',
} as const

const marketNetwork = {
  alt: '',
  src: '/assets/market/bitcoin-network.svg',
} as const

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
  surfaceImage,
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
                  'min-w-touch gap-xs whitespace-nowrap px-xl font-medium',
                  getOutcomeClass(outcome, marketSlug)
                )}
                key={outcome.side}
                size="large"
                variant="secondary"
              >
                <span>Buy {outcome.label}</span>
                <NanoDotIcon className="text-muted-foreground" />
                <span className="font-mono">{price ?? '—'}</span>
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
      background={
        <>
          <Image
            alt={surfaceImage.alt}
            className="object-cover"
            fill
            loading="eager"
            sizes="(min-width: 1024px) 67vw, 100vw"
            src={surfaceImage.src}
          />
          <div className="absolute inset-0 bg-background/50" />
        </>
      }
      identity={
        <div className="flex min-w-0 items-start gap-md">
          {teams === undefined ? (
            <div className="flex size-avatar-lg shrink-0 items-center justify-center rounded-full border border-border-subtle">
              <Image alt={marketNetwork.alt} height={36} src={marketNetwork.src} width={36} />
            </div>
          ) : (
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
          <h1 className="min-w-0 truncate text-display-md font-light text-foreground">{title}</h1>
        </div>
      }
      subtitle={range ? <MarketHeaderSubtitle marketSlug={marketSlug} range={range} /> : null}
    />
  )
}
