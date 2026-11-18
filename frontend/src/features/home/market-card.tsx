import { cn } from '@tiny/ui/lib/cn'
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
              <div
                className={cn({
                  'bg-asset-lakers': marketImage.tone === 'lakers',
                  'bg-surface-raised': marketImage.tone !== 'lakers',
                  'flex size-avatar-md items-center justify-center overflow-hidden rounded-pill': true,
                })}
              >
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
        'flex w-full max-w-market-card min-w-0 flex-col gap-3xl rounded-sm border-[0.5px] border-border-subtle bg-surface-subtle p-xl': true,
        [className ?? '']: className !== undefined,
      })}
      data-slot="market-card"
    >
      <header className="flex flex-col gap-md" data-slot="market-card-header">
        {artwork ? <MarketCardArtwork artwork={artwork} /> : null}
        <h2 className="text-heading-md font-regular">
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
              aria-hidden="true"
              className={cn({
                'border-market-away bg-market-away/80': outcome.side === 'away',
                'border-market-home bg-market-home/80': outcome.side === 'home',
                'size-sm rounded-pill border-[0.5px]': true,
              })}
            />
            <span className="font-mono">
              {outcome.priceCents === undefined
                ? outcome.label
                : `${outcome.label} ${outcome.priceCents}¢`}
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
