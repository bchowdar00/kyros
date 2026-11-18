'use client'

import { cents, createMoneyFormatter, estimateWinningPayout } from '@tiny/money'

import { getPriceStream, IDLE_PRICE_STREAM } from '@/features/exchange-streams/price-stream'
import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'
import { seriesSignalId, useStream } from '../bindings/binding'
import type { ChatData } from '../bindings/chat-data'
import { probabilityToCents } from '../resolve/cards'
import type { PickerSpec, ScenarioSpec, TicketSpec } from '../specs'
import { Picker } from '../widgets/picker'
import { ScenarioSlider, type SliderReadout } from '../widgets/scenario-slider'
import { TicketForm, type TicketOrder } from '../widgets/ticket-form'
import { WidgetSkeleton, WidgetUnavailable } from './frames'

const money = createMoneyFormatter('en-US')
const DEFAULT_STAKE_CENTS = 2_500
const MOVE_RANGE_DOLLARS = 300
const STAKE_RANGE_CENTS = 20_000

/** A sentence the conversation can carry when a widget fires. */
export type WidgetEvent = (text: string) => void

/** The ask each side pays right now, in whole cents, from the market's price signal. */
function useAsks(data: ChatData, series: string) {
  const id = seriesSignalId(series, 'price')
  const signals = useSignalsStream(data.apiUrl, series, [id])
  const frame = signals.frames[id]
  const ask = probabilityToCents(frame, 'ask')
  const bid = probabilityToCents(frame, 'bid')
  if (ask === undefined || bid === undefined) return undefined
  return { YES: Math.round(ask), NO: Math.round(100 - bid) }
}

export function TicketWidget({
  spec,
  data,
  onEvent,
}: {
  spec: TicketSpec
  data: ChatData
  onEvent: WidgetEvent
}) {
  const asks = useAsks(data, spec.series)
  const label = spec.title ?? `Confirm ${spec.side} order`
  if (data.address === null) return <WidgetUnavailable label={label} reason="Sign in to trade." />
  if (asks === undefined) return <WidgetSkeleton label={label} />
  const submit = (order: TicketOrder) => {
    onEvent(
      `Confirmed: buy ${order.side} for ${money.usd(cents(order.amountCents))} at ${order.priceCents}¢ on ${spec.series}.`
    )
  }
  return (
    <TicketForm
      label={label}
      title={spec.title ?? 'Confirm order'}
      prices={asks}
      defaultSide={spec.side}
      defaultAmountCents={spec.amount_cents ?? DEFAULT_STAKE_CENTS}
      disabledReason="Placing orders from chat is not connected yet"
      onSubmit={submit}
    />
  )
}

export function PickerWidget({ spec, onEvent }: { spec: PickerSpec; onEvent: WidgetEvent }) {
  return (
    <Picker
      label={spec.title ?? spec.prompt}
      {...(spec.title === undefined ? {} : { title: spec.title })}
      prompt={spec.prompt}
      options={spec.options.map(option => ({
        id: option.id,
        title: option.title,
        ...(option.description === undefined ? {} : { description: option.description }),
        ...(option.meta === undefined ? {} : { meta: option.meta }),
      }))}
      {...(spec.default_option_id === undefined ? {} : { defaultOptionId: spec.default_option_id })}
      {...(spec.confirm_label === undefined ? {} : { confirmLabel: spec.confirm_label })}
      onSelect={optionId => {
        const option = spec.options.find(candidate => candidate.id === optionId)
        onEvent(`Chose: ${option?.title ?? optionId}.`)
      }}
    />
  )
}

/**
 * Named derivations, arithmetic only. `btc_move` settles the round for a move from the
 * strike and prices the stake at today's ask; `stake` prices a stake at today's ask. The
 * readouts are the answer; dragging sends nothing back, so a slider never floods the chat.
 */
export function ScenarioWidget({ spec, data }: { spec: ScenarioSpec; data: ChatData }) {
  const asks = useAsks(data, spec.series)
  const price = useStream(getPriceStream(data.apiUrl, spec.series), IDLE_PRICE_STREAM)
  const side = spec.side ?? 'YES'
  const stakeCents = spec.stake_cents ?? DEFAULT_STAKE_CENTS
  const label =
    spec.title ?? (spec.variable === 'btc_move' ? 'What if BTC moves' : 'What if I stake')
  if (asks === undefined) return <WidgetSkeleton label={label} />
  const ask = asks[side]
  const payout = (stake: number) => estimateWinningPayout(cents(stake), cents(ask))

  if (spec.variable === 'btc_move') {
    const strike = price.round?.strike
    if (strike === undefined)
      return <WidgetUnavailable label={label} reason="No round is open to settle against." />
    const derive = (move: number): SliderReadout[] => {
      const wins = move >= 0 ? side === 'YES' : side === 'NO'
      const pnl = wins ? payout(stakeCents) - stakeCents : -stakeCents
      return [
        { id: 'settles', label: 'Settles', value: move >= 0 ? 'UP' : 'DOWN' },
        {
          id: 'close',
          label: 'Close price',
          value: money.usd(cents(Math.round((strike + move) * 100))),
        },
        { id: 'stake', label: `Stake on ${side} at ${ask}¢`, value: money.usd(cents(stakeCents)) },
        {
          id: 'pnl',
          label: 'P&L at settlement',
          value: money.usd(cents(pnl), { signDisplay: 'always' }),
          tone: pnl >= 0 ? 'positive' : 'negative',
        },
      ]
    }
    return (
      <ScenarioSlider
        label={label}
        title={label}
        name="BTC move from the strike at close"
        min={-MOVE_RANGE_DOLLARS}
        max={MOVE_RANGE_DOLLARS}
        step={5}
        defaultValue={0}
        format={move => money.usd(cents(move * 100), { signDisplay: 'always' })}
        derive={derive}
        onChange={() => undefined}
      />
    )
  }

  const derive = (stake: number): SliderReadout[] => [
    { id: 'price', label: `${side} ask`, value: `${ask}¢` },
    { id: 'payout', label: `Pays if ${side}`, value: money.usd(payout(stake)) },
    {
      id: 'profit',
      label: 'Profit if right',
      value: money.usd(cents(payout(stake) - stake), { signDisplay: 'always' }),
      tone: 'positive',
    },
    {
      id: 'loss',
      label: 'Loss if wrong',
      value: money.usd(cents(-stake), { signDisplay: 'always' }),
      tone: 'negative',
    },
  ]
  return (
    <ScenarioSlider
      label={label}
      title={label}
      name={`Stake on ${side}`}
      min={100}
      max={STAKE_RANGE_CENTS}
      step={100}
      defaultValue={stakeCents}
      format={stake => money.usd(cents(stake))}
      derive={derive}
      onChange={() => undefined}
    />
  )
}
