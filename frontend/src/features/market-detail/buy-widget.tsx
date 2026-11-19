'use client'

import { formatExchangeTicks } from '@kyros/money'
import type { MarketTrading } from './exchange/buy'
import { Button } from '@kyros/ui/components/button'
import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { cn } from '@kyros/ui/lib/cn'

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
      <div className="flex items-center justify-between text-label font-semibold text-muted-foreground">
        <div className="flex items-center gap-xl">
          <span className="font-display text-display-sm font-bold tracking-heading text-foreground">
            Buy by hand
          </span>
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
                'h-auto min-w-0 flex-1 flex-col items-start gap-0 rounded-md border-2 px-lg pb-md pt-sm text-left',
                {
                  'border-action bg-action text-on-action hover:bg-action hover:text-on-action':
                    selected && positive,
                  'border-foreground bg-foreground text-background hover:bg-foreground hover:text-background':
                    selected && !positive,
                  'border-action bg-transparent text-positive hover:bg-surface hover:text-positive':
                    !selected && positive,
                  'border-foreground bg-transparent text-foreground hover:bg-surface':
                    !selected && !positive,
                }
              )}
            >
              <span className="font-display text-heading-sm font-bold capitalize">
                {outcome.label.toLowerCase()}
              </span>
              <span className="font-display text-price font-extrabold tracking-display">
                {outcome.priceTicks !== undefined
                  ? formatExchangeTicks(outcome.priceTicks)
                  : outcome.priceCents === undefined
                    ? '—'
                    : `${outcome.priceCents}¢`}
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
        <p className="text-label font-medium text-muted-foreground">
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
        className="min-h-app-nav w-full rounded-md border-foreground bg-foreground font-display text-heading-sm font-bold text-background hover:border-action hover:bg-action hover:text-on-action"
      >
        Buy
      </Button>
    </dialog>
  )
}
