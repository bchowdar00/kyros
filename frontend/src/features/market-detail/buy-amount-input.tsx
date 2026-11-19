'use client'

import { cents, createMoneyFormatter, type Cents } from '@kyros/money'
import { Button } from '@kyros/ui/components/button'

interface BuyAmountInputProps {
  amount: string
  onAmountChange: (amount: string) => void
  availableBalanceCents?: Cents
}

const money = createMoneyFormatter('en-US')
const increments = [100, 500, 1_000, 2_500] as const
const decimalAmount = /^\d*(?:\.\d{0,2})?$/

function parseAmount(amount: string): Cents | undefined {
  if (amount.length > 16 || !decimalAmount.test(amount)) return undefined
  const [whole = '', fraction = ''] = amount.split('.')
  const minorUnits = BigInt(whole || '0') * 100n + BigInt(fraction.padEnd(2, '0'))
  return minorUnits <= BigInt(Number.MAX_SAFE_INTEGER) ? cents(Number(minorUnits)) : undefined
}

function amountText(amount: Cents): string {
  const formatted = money.usd(amount).slice(1).replaceAll(',', '')
  return formatted.endsWith('.00') ? formatted.slice(0, -3) : formatted
}

export function BuyAmountInput({
  amount,
  onAmountChange,
  availableBalanceCents,
}: BuyAmountInputProps) {
  const parsedAmount = parseAmount(amount)

  return (
    <div className="flex flex-col gap-xl py-lg">
      <label className="flex min-h-trade-amount min-w-0 items-center justify-between gap-xl">
        <span className="text-body font-semibold text-muted-foreground">How much</span>
        <input
          aria-label="Order amount in USD"
          inputMode="decimal"
          value={amount === '' ? '' : `$${amount}`}
          onChange={event => {
            const draft = event.currentTarget.value.replace('$', '')
            if (parseAmount(draft) !== undefined) onAmountChange(draft)
          }}
          placeholder="$0"
          className="min-w-0 flex-1 rounded-md border-[1.5px] border-foreground bg-transparent px-md py-xs text-right font-display text-display-md font-bold tracking-display text-foreground placeholder:text-muted-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
        />
      </label>
      <fieldset className="flex flex-wrap justify-end gap-sm" aria-label="Adjust order amount">
        {increments.map(increment => (
          <Button
            key={increment}
            variant="secondary"
            size="small"
            className="relative min-w-touch after:absolute after:inset-x-0 after:top-1/2 after:min-h-touch after:-translate-y-1/2 after:content-['']"
            disabled={parsedAmount === undefined || !Number.isSafeInteger(parsedAmount + increment)}
            onClick={() => {
              if (parsedAmount !== undefined)
                onAmountChange(amountText(cents(parsedAmount + increment)))
            }}
          >
            +${amountText(cents(increment))}
          </Button>
        ))}
        <Button
          variant="secondary"
          size="small"
          className="relative min-w-touch after:absolute after:inset-x-0 after:top-1/2 after:min-h-touch after:-translate-y-1/2 after:content-['']"
          disabled={availableBalanceCents === undefined}
          onClick={() => {
            if (availableBalanceCents !== undefined)
              onAmountChange(amountText(availableBalanceCents))
          }}
        >
          Max
        </Button>
      </fieldset>
    </div>
  )
}
