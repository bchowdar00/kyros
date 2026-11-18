'use client'

import { Button } from '@tiny/ui/components/button'
import { cents, createMoneyFormatter, estimateWinningPayout } from '@tiny/money'
import { cn } from '@tiny/ui/lib/cn'
import { type FormEvent, useId, useState } from 'react'

import { WidgetFrame } from '../widget-frame'

export type TicketSide = 'YES' | 'NO'

export interface TicketOrder {
  side: TicketSide
  /** Stake in integer cents. */
  amountCents: number
  /** The price the ticket quoted, in integer cents per share. */
  priceCents: number
}

export interface TicketFormProps {
  label: string
  title?: string
  /** Current ask per side, in cents. */
  prices: Record<TicketSide, number>
  defaultSide?: TicketSide
  defaultAmountCents?: number
  /** Why the ticket cannot be sent right now. Present means disabled. */
  disabledReason?: string
  onSubmit: (order: TicketOrder) => void
}

const money = createMoneyFormatter('en-US')
const QUICK_AMOUNTS_CENTS = [1_000, 2_500, 5_000, 10_000]

/** A trade confirmation inside the reply. It sends one event back and then goes quiet. */
export function TicketForm({
  label,
  title = 'Confirm order',
  prices,
  defaultSide = 'YES',
  defaultAmountCents = 2_500,
  disabledReason,
  onSubmit,
}: TicketFormProps) {
  const [side, setSide] = useState<TicketSide>(defaultSide)
  const [dollars, setDollars] = useState((defaultAmountCents / 100).toFixed(2))
  const [sent, setSent] = useState<TicketOrder | null>(null)
  const amountId = useId()
  const amountCents = Math.round(Number.parseFloat(dollars || '0') * 100)
  const valid = Number.isFinite(amountCents) && amountCents > 0
  const price = prices[side]
  const payout = valid ? estimateWinningPayout(cents(amountCents), cents(price)) : cents(0)

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (!valid || disabledReason) return
    const order = { side, amountCents, priceCents: price }
    setSent(order)
    onSubmit(order)
  }

  if (sent) {
    return (
      <WidgetFrame label={label} title={title}>
        <output className="text-body text-foreground">
          Sent. Buy {sent.side} for {money.usd(cents(sent.amountCents))} at {sent.priceCents}¢.
        </output>
      </WidgetFrame>
    )
  }

  return (
    <WidgetFrame label={label} title={title}>
      <form onSubmit={submit} className="flex flex-col gap-lg">
        <fieldset className="grid grid-cols-2 gap-sm">
          <legend className="sr-only">Side</legend>
          {(['YES', 'NO'] as const).map(option => (
            <label
              key={option}
              className={cn(
                'flex cursor-pointer items-center justify-between rounded-md border-[0.5px] px-lg py-md text-body transition-colors has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-focus-ring',
                {
                  'border-positive/50 bg-positive-muted text-foreground':
                    side === option && option === 'YES',
                  'border-negative/50 bg-negative-muted text-foreground':
                    side === option && option === 'NO',
                  'border-border-subtle text-muted-foreground hover:text-foreground':
                    side !== option,
                }
              )}
            >
              <input
                type="radio"
                name={`${amountId}-side`}
                value={option}
                checked={side === option}
                onChange={() => setSide(option)}
                className="sr-only"
              />
              <span>{option}</span>
              <span className="font-mono">{prices[option]}¢</span>
            </label>
          ))}
        </fieldset>
        <div className="flex flex-col gap-sm">
          <label htmlFor={amountId} className="text-caption text-muted-foreground">
            Amount
          </label>
          <div className="flex items-center gap-sm rounded-md border-[0.5px] border-border-subtle bg-surface px-md focus-within:border-foreground">
            <span className="font-mono text-body text-muted-foreground">$</span>
            <input
              id={amountId}
              inputMode="decimal"
              value={dollars}
              onChange={event => setDollars(event.currentTarget.value)}
              className="min-h-touch min-w-0 flex-1 bg-transparent font-mono text-heading-sm text-foreground outline-none"
            />
          </div>
          <div className="flex gap-xs">
            {QUICK_AMOUNTS_CENTS.map(quick => (
              <Button
                key={quick}
                size="small"
                variant="ghost"
                onClick={() => setDollars((quick / 100).toFixed(2))}
              >
                {money.usd(cents(quick), { maximumFractionDigits: 0 })}
              </Button>
            ))}
          </div>
        </div>
        <p className="flex justify-between text-label text-muted-foreground">
          <span>Pays if {side}</span>
          <span className="font-mono text-foreground">{money.usd(payout)}</span>
        </p>
        <Button type="submit" size="large" disabled={!valid || disabledReason !== undefined}>
          {disabledReason ?? `Buy ${side}`}
        </Button>
      </form>
    </WidgetFrame>
  )
}
