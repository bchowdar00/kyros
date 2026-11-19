import { createMoneyFormatter, formatExchangeTicks, type Cents } from '@kyros/money'

interface BuyPayoutSummaryProps {
  priceTicks?: number
  payoutCents?: Cents
  averagePriceCents?: Cents
}

const money = createMoneyFormatter('en-US')

export function BuyPayoutSummary({
  payoutCents,
  averagePriceCents,
  priceTicks,
}: BuyPayoutSummaryProps) {
  return (
    <div className="-mx-xl flex items-center justify-between gap-lg border-t border-border-subtle px-xl pt-xl pb-sm">
      <div className="flex flex-col gap-sm">
        <span className="text-body font-semibold text-foreground">You win if right</span>
        <p className="text-label text-muted-foreground">
          {priceTicks === undefined ? 'Avg. price' : 'Price cap'}{' '}
          <span className="font-mono">
            {priceTicks !== undefined
              ? formatExchangeTicks(priceTicks)
              : averagePriceCents === undefined
                ? '—'
                : money.price(averagePriceCents)}
          </span>
        </p>
      </div>
      <output
        aria-label="Potential payout"
        className="font-display text-display-md font-extrabold tracking-display text-foreground"
      >
        {payoutCents === undefined ? (
          <>
            <span aria-hidden="true">—</span>
            <span className="sr-only">Unknown</span>
          </>
        ) : (
          money.usd(payoutCents)
        )}
      </output>
    </div>
  )
}
