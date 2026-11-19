import {
  CENTS_PER_DOLLAR,
  cents,
  createMoneyFormatter,
  estimateWinningPayout,
  estimateExchangeWinningPayout,
  isCents,
} from '@kyros/money'
import { MiniDoubleForwardIcon } from '@kyros/ui/icons/mini-double-forward-icon'
import { cn } from '@kyros/ui/lib/cn'
import type { ComponentPropsWithRef } from 'react'

interface MarketPayoutPreviewProps extends ComponentPropsWithRef<'output'> {
  outcomeLabel: string
  priceCents?: number | undefined
  priceTicks?: number | undefined
}

const PREVIEW_STAKE = cents(10_000)
const money = createMoneyFormatter('en-US')

export function MarketPayoutPreview({
  outcomeLabel,
  priceCents,
  priceTicks,
  className,
  ...props
}: MarketPayoutPreviewProps) {
  const centsPayout =
    isCents(priceCents) && priceCents > 0 && priceCents <= CENTS_PER_DOLLAR
      ? estimateWinningPayout(PREVIEW_STAKE, priceCents)
      : undefined
  const payout =
    priceTicks === undefined
      ? centsPayout
      : Number.isSafeInteger(priceTicks) && priceTicks > 0 && priceTicks <= 1000
        ? estimateExchangeWinningPayout(PREVIEW_STAKE, priceTicks)
        : undefined

  return (
    <output
      {...props}
      aria-label={`Potential payout for ${outcomeLabel}`}
      className={cn(
        'flex items-center justify-center gap-xs whitespace-nowrap font-mono text-label text-muted-foreground',
        className
      )}
    >
      <span>{money.usd(PREVIEW_STAKE, { maximumFractionDigits: 0 })}</span>
      <span aria-hidden="true" className="relative inline-flex size-lg shrink-0">
        <MiniDoubleForwardIcon />
        <MiniDoubleForwardIcon className="payout-shine-mask absolute inset-0 hidden opacity-15 motion-safe:block motion-safe:animate-payout-shine" />
      </span>
      <span className="sr-only">could return </span>
      <span>{payout === undefined ? '—' : money.usd(payout, { maximumFractionDigits: 0 })}</span>
      <span className="sr-only"> if {outcomeLabel} wins</span>
    </output>
  )
}
