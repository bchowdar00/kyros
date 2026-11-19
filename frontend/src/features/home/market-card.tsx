import { cn } from '@kyros/ui/lib/cn'
import Image from 'next/image'
import Link from 'next/link'
import type { ComponentPropsWithRef, ReactNode } from 'react'

import type { HomeMarketOutcome, MarketCardArtwork } from './types'

export interface MarketCardProps extends ComponentPropsWithRef<'section'> {
  artwork?: MarketCardArtwork | undefined
  category: string
  chart: ReactNode
  marketStatus?: string | undefined
  outcomes: readonly [HomeMarketOutcome, HomeMarketOutcome]
  slug: string
  title: string
}

/* The exchange names its binary sides in capitals; a card says them as the market page does. */
const spokenOutcomeLabels: Record<string, string> = {
  DOWN: 'Down',
  NO: 'No',
  UP: 'Up',
  YES: 'Yes',
}

function MarketCardArtwork({ artwork }: { artwork: MarketCardArtwork }) {
  return (
    <figure
      aria-label={artwork.label}
      className="flex h-avatar-lg items-center"
      data-slot="market-card-artwork"
    >
      {artwork.images.map((marketImage, imageIndex) => {
        const inset = marketImage.presentation === 'inset'

        return (
          <div
            className={cn({
              '-mr-xl': artwork.images.length > 1 && imageIndex === 0,
              relative: true,
            })}
            key={marketImage.src}
          >
            <div
              aria-hidden="true"
              className="flex size-avatar-lg shrink-0 items-center justify-center rounded-pill border border-highlight bg-background"
              data-slot="market-card-thumbnail"
            >
              <div className="flex size-avatar-md items-center justify-center overflow-hidden rounded-pill bg-surface-raised">
                <Image
                  alt=""
                  className={inset ? 'h-xl w-2xl' : 'size-avatar-md'}
                  draggable={false}
                  height={inset ? 20 : 36}
                  sizes="36px"
                  src={marketImage.src}
                  unoptimized
                  width={inset ? 24 : 36}
                />
              </div>
            </div>
          </div>
        )
      })}
    </figure>
  )
}

/** A presentational market summary with a pluggable chart and single or team artwork. */
export function MarketCard({
  artwork,
  category,
  chart,
  className,
  marketStatus,
  outcomes,
  slug,
  title,
  ...props
}: MarketCardProps) {
  return (
    <section
      {...props}
      aria-label={props['aria-label'] ?? title}
      className={cn({
        'flex w-full max-w-market-card min-w-0 flex-col gap-2xl rounded-md border-2 border-foreground bg-background p-xl': true,
        [className ?? '']: className !== undefined,
      })}
      data-slot="market-card"
    >
      <header className="flex flex-col gap-md" data-slot="market-card-header">
        {artwork ? <MarketCardArtwork artwork={artwork} /> : null}
        <h2 className="font-display text-heading-md font-extrabold tracking-heading">
          <Link
            className="text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
            href={`/market/${slug}`}
          >
            {title}
          </Link>
        </h2>
      </header>

      <ul className="flex items-center justify-between gap-md" data-slot="market-card-outcomes">
        {outcomes.map((outcome, outcomeIndex) => (
          <li
            className={cn({
              'flex items-center gap-md text-body font-medium': true,
              'flex-row': outcomeIndex === 0,
              'flex-row-reverse': outcomeIndex !== 0,
            })}
            data-slot="market-card-outcome"
            key={outcome.side}
          >
            <span
              className={cn({
                'font-mono font-bold': true,
                'text-foreground': outcome.side === 'away',
                'text-positive': outcome.side === 'home',
              })}
            >
              {outcome.priceCents === undefined
                ? (spokenOutcomeLabels[outcome.label] ?? outcome.label)
                : `${spokenOutcomeLabels[outcome.label] ?? outcome.label} ${outcome.priceCents}¢`}
            </span>
          </li>
        ))}
      </ul>

      <div className="h-market-card-chart w-full" data-slot="market-card-chart">
        {chart}
      </div>
      <footer
        className="flex items-center justify-between gap-md text-body font-regular text-muted-foreground"
        data-slot="market-card-metadata"
      >
        <p>{category}</p>
        {marketStatus ? <p className="truncate">{marketStatus}</p> : null}
      </footer>
    </section>
  )
}
