'use client'

import { useState } from 'react'

import { cents, createMoneyFormatter } from '@tiny/money'

import { Picker } from '../widgets/picker'
import { ScenarioSlider } from '../widgets/scenario-slider'

export function PickerExample() {
  return (
    <Picker
      label="Choose a strategy version"
      title="Which version?"
      prompt="Three versions match what you described. Deploy which one?"
      options={[
        {
          id: 'v1',
          title: 'V1, price only',
          description: 'Buy the side that dropped 12¢ inside 3 minutes.',
          meta: '58% win',
        },
        {
          id: 'v2',
          title: 'V2, with a score filter',
          description: 'Same entry, only while the score is within 6 points.',
          meta: '63% win',
        },
        {
          id: 'v3',
          title: 'V3, with an RSI exit',
          description: 'V2 plus a cancel when RSI crosses back above 50.',
          meta: '61% win',
        },
      ]}
      defaultOptionId="v2"
      confirmLabel="Deploy this version"
      onSelect={() => undefined}
    />
  )
}
import { TicketForm, type TicketOrder } from '../widgets/ticket-form'

const money = createMoneyFormatter('en-US')
/** Position held in the example: 100 YES shares bought at 58¢. */
const HELD_SHARES = 100
const ENTRY_CENTS = 58

/** Fair YES probability for a BTC move from the strike, a logistic in dollars. */
function fairYesCents(moveDollars: number) {
  return Math.round(100 / (1 + Math.exp(-moveDollars / 60)))
}

export function SliderExample() {
  return (
    <ScenarioSlider
      label="Scenario: BTC move from here"
      title="What if BTC moves"
      name="Move from strike at close"
      min={-300}
      max={300}
      step={10}
      defaultValue={40}
      format={value => `${value > 0 ? '+' : value < 0 ? '−' : ''}$${Math.abs(value)}`}
      derive={move => {
        const fair = fairYesCents(move)
        const settles = move >= 0
        const pnlCents = settles ? HELD_SHARES * (100 - ENTRY_CENTS) : -HELD_SHARES * ENTRY_CENTS
        const markCents = HELD_SHARES * (fair - ENTRY_CENTS)
        return [
          { id: 'fair', label: 'Fair YES now', value: `${fair}¢` },
          {
            id: 'mark',
            label: 'Mark to fair',
            value: money.usd(cents(markCents), { signDisplay: 'always' }),
            tone: markCents >= 0 ? 'positive' : 'negative',
          },
          { id: 'settle', label: 'Resolves', value: settles ? 'YES' : 'NO' },
          {
            id: 'pnl',
            label: 'P&L at settlement',
            value: money.usd(cents(pnlCents), { signDisplay: 'always' }),
            tone: pnlCents >= 0 ? 'positive' : 'negative',
          },
        ]
      }}
      onChange={() => undefined}
    />
  )
}

export function TicketExample() {
  const [, setLastOrder] = useState<TicketOrder | null>(null)
  return (
    <TicketForm
      label="Trade ticket"
      title="Buy on BTC 2m"
      prices={{ YES: 58, NO: 43 }}
      onSubmit={setLastOrder}
    />
  )
}
