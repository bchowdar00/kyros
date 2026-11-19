'use client'

import {
  AnimatedNumber,
  AnimatedNumberGroup,
  type AnimatedNumberFormat,
} from '@kyros/ui/components/animated-number'
import type { ReactNode } from 'react'

import { cn } from '@kyros/ui/lib/cn'
import { CENTS_PER_DOLLAR, createMoneyFormatter, isCents } from '@kyros/money'
import type { ChartFeed } from '@kyros/ui/charts/lib/types'

/** One shape for both renderers, so the rolling digits and the read-aloud value cannot drift. */
const PRICE_FORMAT = {
  currency: 'USD',
  maximumFractionDigits: 2,
  minimumFractionDigits: 2,
  style: 'currency',
} as const satisfies AnimatedNumberFormat

/** Symbols stay outside the digit reels, so sign flips cannot leave fading duplicates. */
const DIGIT_FORMAT = { ...PRICE_FORMAT, style: 'decimal' } as const satisfies AnimatedNumberFormat
const LOCALE = 'en-US'
const money = createMoneyFormatter(LOCALE)

function formatAmount(amount: number, signed = false) {
  return isCents(amount) ? money.usd(amount, { signDisplay: signed ? 'exceptZero' : 'auto' }) : '—'
}

/**
 * A price tick is the explanation, not decoration (AGENTS.md §4), so the roll is
 * quick and the digits carry the movement rather than a fade.
 */
const FLOW_TIMING = {
  opacityTiming: { duration: 180, easing: 'ease-out' },
  spinTiming: { duration: 180, easing: 'cubic-bezier(0.2, 0, 0, 1)' },
  transformTiming: { duration: 180, easing: 'cubic-bezier(0.2, 0, 0, 1)' },
} as const

export interface MarketPriceSummaryProps {
  className?: string
  feed?: ChartFeed
  /** Latest live value. Chart hover state is intentionally independent. */
  price: number
  /** What the market resolves against, for example `Price to beat`. */
  targetLabel: string
  targetPrice: number
  /** Remaining market duration, formatted by the application that owns the market clock. */
  timeLeft?: ReactNode
}

function getFeedStatus(feed: ChartFeed | undefined) {
  switch (feed?.state) {
    case 'disconnected':
      return 'feed disconnected'
    case 'paused':
      return 'feed paused'
    default:
      return undefined
  }
}

/** Flat is its own state: no direction to report, so no direction colour. */
function getDirectionClass(delta: number) {
  if (delta > 0) {
    return 'text-positive'
  }

  if (delta < 0) {
    return 'text-negative'
  }

  return 'text-foreground'
}

function StatDivider() {
  return <div aria-hidden="true" className="h-chart-stat-rule w-px shrink-0 bg-border-subtle" />
}

/**
 * A number that rolls to its next value, with the width of the change animated
 * rather than jumped. An unresolved value stays unknown instead of rendering as
 * a number.
 *
 * NumberFlow renders every digit 0-9 into a shadow root and hides all but one with
 * a transform, so the visual is no use to a screen reader. `label` is the same value
 * as one string, and is what assistive technology actually reads.
 */
function FlowValue({
  label,
  signed = false,
  value,
}: {
  label: string
  signed?: boolean
  value: number
}) {
  if (!Number.isFinite(value)) {
    return <>—</>
  }

  return (
    <>
      <span className="sr-only">{label}</span>
      <span aria-hidden="true" className="inline-flex items-center whitespace-nowrap">
        {signed ? (
          <span className={cn({ 'opacity-0': value === 0 })}>{value < 0 ? '−' : '+'}</span>
        ) : null}
        <span>$</span>
        <AnimatedNumber
          {...FLOW_TIMING}
          format={DIGIT_FORMAT}
          locales={LOCALE}
          value={Math.abs(value)}
        />
      </span>
    </>
  )
}

function Stat({
  children,
  label,
  valueClassName,
}: {
  children: ReactNode
  label: string
  valueClassName?: string
}) {
  return (
    <div className="min-w-0 flex-1 px-lg first:pl-0">
      <dt className="text-label font-semibold text-muted-foreground">{label}</dt>
      <dd
        className={cn(
          'm-0 mt-xs font-display text-display-md font-extrabold tracking-display',
          valueClassName
        )}
      >
        {children}
      </dd>
    </div>
  )
}

/**
 * The four-stat header above a price chart: the target, current price, change,
 * and time remaining for the market to resolve.
 */
export function MarketPriceSummary({
  className,
  feed,
  price,
  targetLabel,
  targetPrice,
  timeLeft,
}: MarketPriceSummaryProps) {
  // Chart coordinates enter as dollars; all headline arithmetic uses integer cents.
  const priceCents = Math.round(price * CENTS_PER_DOLLAR)
  const targetCents = Math.round(targetPrice * CENTS_PER_DOLLAR)
  const currentPrice = priceCents / CENTS_PER_DOLLAR
  const target = targetCents / CENTS_PER_DOLLAR
  const delta = (priceCents - targetCents) / CENTS_PER_DOLLAR
  const directionClass = getDirectionClass(delta)
  const feedStatus = getFeedStatus(feed)

  return (
    <header aria-label="Market price summary" className={cn('flex flex-col gap-lg', className)}>
      {/* The same four figures as one plain sentence; the stats below stay the exact record.
          Only a live feed may say "right now": a stopped feed's last price is not the price. */}
      {Number.isFinite(delta) && feedStatus === undefined ? (
        <p className="max-w-content font-display text-display-sm font-medium text-foreground">
          Right now BTC is{' '}
          <b className="whitespace-nowrap font-extrabold">
            {delta === 0
              ? 'level with'
              : `${formatAmount(Math.abs(priceCents - targetCents))} ${delta > 0 ? 'above' : 'below'}`}
          </b>{' '}
          the {targetLabel.toLowerCase()}.{' '}
          {delta === 0 ? null : (
            <b className={cn('font-extrabold', directionClass)}>
              {delta > 0 ? 'Up is ahead.' : 'Down is ahead.'}
            </b>
          )}
        </p>
      ) : null}
      <AnimatedNumberGroup>
        <dl className="flex min-w-0 flex-1 items-center">
          <Stat label={targetLabel} valueClassName="text-foreground">
            <FlowValue label={formatAmount(targetCents)} value={target} />
          </Stat>

          <StatDivider />

          <Stat label="Current" valueClassName="text-foreground">
            <FlowValue label={formatAmount(priceCents)} value={currentPrice} />
          </Stat>

          <StatDivider />

          <Stat
            label={`Change${feedStatus ? ` · ${feedStatus}` : ''}`}
            valueClassName={directionClass}
          >
            <FlowValue signed label={formatAmount(priceCents - targetCents, true)} value={delta} />
          </Stat>

          <StatDivider />

          <Stat label="Time Left" valueClassName="text-foreground">
            {timeLeft ?? '—'}
          </Stat>
        </dl>
      </AnimatedNumberGroup>
    </header>
  )
}
