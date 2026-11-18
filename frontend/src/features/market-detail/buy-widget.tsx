'use client'

import { formatExchangeTicks } from '@tiny/money'
import type { MarketTrading } from './exchange/buy'
import { Button } from '@tiny/ui/components/button'
import { NanoDotIcon } from '@tiny/ui/icons/nano-dot-icon'
import { NanoChevronIcon } from '@tiny/ui/icons/nano-chevron-icon'
import { cn } from '@tiny/ui/lib/cn'

import { BuyPayoutSummary } from './buy-payout-summary'
import { BuyAmountInput } from './buy-amount-input'
import { WidgetBackButton } from './widget-back-button'
import type { MarketOutcome, MarketSide } from './types'

interface BuyWidgetProps {
  trading?: MarketTrading | undefined
  amount: string
  onAmountChange: (amount: string) => void
  outcomes: readonly MarketOutcome[]
  selectedSide: MarketSide
  onSelect: (side: MarketSide) => void
  onClose: () => void
}

export function BuyWidget({
  trading,
  amount,
  onAmountChange,
  outcomes,
  selectedSide,
  onSelect,
  onClose,
}: BuyWidgetProps) {
  const quote = trading?.quote(amount, selectedSide)
  return (
    <dialog
      open
      aria-label="Buy outcome"
      className="relative m-0 flex w-full max-w-none flex-col gap-xl border-0 bg-transparent p-xl text-foreground"
    >
      <WidgetBackButton onClose={onClose} />
      <div className="flex items-center justify-between text-heading-sm">
        <div className="flex items-center gap-xl">
          <span className="text-foreground">Buy</span>
          <button type="button" disabled className="-my-sort-hit min-h-touch text-muted-foreground">
            Sell
          </button>
        </div>
        <span className="flex items-center gap-copy">
          {trading ? 'Price-capped' : 'Market'} <NanoChevronIcon />
        </span>
      </div>
      <fieldset className="flex gap-sm" aria-label="Outcome">
        {outcomes.map(outcome => {
          const selected = outcome.side === selectedSide
          const positive = outcome.side === 'home'
          return (
            <Button
              key={outcome.side}
              aria-label={`Select ${outcome.label}`}
              aria-pressed={selected}
              onClick={() => onSelect(outcome.side)}
              variant="ghost"
              className={cn(
                'min-h-app-nav min-w-0 flex-1 gap-0 rounded-sm px-xl text-body font-medium',
                {
                  'border-positive bg-positive/80 text-foreground hover:bg-positive hover:text-foreground':
                    selected && positive,
                  'border-negative bg-negative/80 text-foreground hover:bg-negative/80':
                    selected && !positive,
                  'border-positive/10 bg-positive/25 hover:bg-positive/30': !selected && positive,
                  'border-negative/10 bg-negative/25 hover:bg-negative/30': !selected && !positive,
                }
              )}
            >
              <span className={cn('flex items-center gap-0', { 'text-foreground/60': !selected })}>
                <span className="capitalize">{outcome.label.toLowerCase()}</span>
                <NanoDotIcon />
                <span className="font-mono">
                  {outcome.priceTicks !== undefined
                    ? formatExchangeTicks(outcome.priceTicks)
                    : outcome.priceCents === undefined
                      ? '—'
                      : `${outcome.priceCents}¢`}
                </span>
              </span>
            </Button>
          )
        })}
      </fieldset>
      <BuyAmountInput
        amount={amount}
        onAmountChange={onAmountChange}
        {...(trading?.availableBalanceCents === undefined
          ? {}
          : { availableBalanceCents: trading.availableBalanceCents })}
      />
      <BuyPayoutSummary
        {...(quote === undefined
          ? {}
          : { payoutCents: quote.payoutCents, priceTicks: quote.outcomePrice })}
      />
      {trading && (
        <p className="text-label text-muted-foreground">
          {trading.reason ??
            (quote === undefined
              ? 'Enter an affordable amount with available liquidity.'
              : `Up to ${quote.qty} lots. Unfilled lots cancel immediately; unused funds stay in your demo account.`)}
        </p>
      )}
      {trading && <output className="text-label">{trading.notice}</output>}
      <Button
        disabled={quote === undefined || trading?.reason !== undefined}
        onClick={() => {
          if (quote !== undefined) trading?.submit(quote)
        }}
        className="min-h-app-nav w-full rounded-sm text-body font-medium disabled:opacity-100"
      >
        Buy
      </Button>
    </dialog>
  )
}
