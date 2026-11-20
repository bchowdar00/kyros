'use client'

import { createRustExchangeClient } from '@kyros/api'
import { createMoneyFormatter } from '@kyros/money'
import { Badge } from '@kyros/ui/components/badge'
import { Button } from '@kyros/ui/components/button'
import { MiniForwardIcon } from '@kyros/ui/icons/mini-forward-icon'
import { cn } from '@kyros/ui/lib/cn'
import { useMemo, useRef, useState } from 'react'

import { RecordLine } from './record/record-line'
import type { MarketSide, MyStrategy, StrategyStatus } from './types'

export interface MyStrategyRowProps {
  /**
   * Exchange origin for this row's on-chain record line and its pause/stop controls. Only a
   * strategy sub-account has a record (its `id` is the sub-account address); leave it unset on
   * the main account's row, which then shows neither the record line nor any controls.
   */
  apiUrl?: string | undefined
  /**
   * Called when a stop cancelled the orders but left the position open. The exchange then drops
   * the row from the list, so the table, not the row, has to say what happened.
   */
  onStopPending?: ((strategyName: string) => void) | undefined
  /** The owning account address the controls act as; absent signed-out, which hides the controls. */
  parentAddress?: string | undefined
  showDivider?: boolean
  strategy: MyStrategy
}

/** The full-width row shown directly under a strategy, for a confirmation or an outcome message. */
type RowPanel = { kind: 'none' } | { kind: 'confirm' } | { kind: 'error'; message: string }

const positionDotClasses: Record<MarketSide, string> = {
  away: 'border-market-away bg-market-away/80',
  home: 'border-market-home bg-market-home/80',
}

const money = createMoneyFormatter('en-US')

const controlClasses = 'h-lg shrink-0 px-xs py-0 text-caption'

export function MyStrategyRow({
  apiUrl,
  onStopPending,
  parentAddress,
  showDivider = false,
  strategy,
}: MyStrategyRowProps) {
  const client = useMemo(
    () => (apiUrl === undefined ? undefined : createRustExchangeClient({ baseUrl: apiUrl })),
    [apiUrl]
  )
  // A stop that left a position open has already ended the strategy on the exchange: there is
  // nothing left to pause or stop, and the row itself goes with the next account frame.
  const [stopped, setStopped] = useState(false)
  const hasControls = client !== undefined && parentAddress !== undefined && !stopped

  // Displayed status follows the live feed (the `strategy.status` prop) but a successful
  // pause/resume updates it at once, so the badge flips without waiting for the next frame.
  const [status, setStatus] = useState<StrategyStatus>(strategy.status)
  const [syncedStatus, setSyncedStatus] = useState<StrategyStatus>(strategy.status)
  if (strategy.status !== syncedStatus) {
    setSyncedStatus(strategy.status)
    setStatus(strategy.status)
  }

  const [busy, setBusy] = useState<false | 'status' | 'stop'>(false)
  const [panel, setPanel] = useState<RowPanel>({ kind: 'none' })
  const [removed, setRemoved] = useState(false)
  const stopButtonRef = useRef<HTMLButtonElement>(null)

  if (removed) return null

  const paused = status === 'paused'
  const pnlIsPositive = strategy.totalPnlCents >= 0

  const toggleStatus = async () => {
    if (client === undefined || parentAddress === undefined || busy !== false) return
    const next: StrategyStatus = paused ? 'active' : 'paused'
    setBusy('status')
    setPanel({ kind: 'none' })
    // A request that throws (a dropped connection) is a failure like any other: the buttons
    // must come back either way.
    const ok = await client
      .updateStrategyStatus(parentAddress, strategy.id, next)
      .then(result => result.ok)
      .catch(() => false)
    setBusy(false)
    if (ok) {
      setStatus(next)
      return
    }
    setPanel({
      kind: 'error',
      message:
        next === 'paused'
          ? 'Could not pause the strategy. Try again.'
          : 'Could not resume the strategy. Try again.',
    })
  }

  const confirmStop = async () => {
    if (client === undefined || parentAddress === undefined || busy !== false) return
    setBusy('stop')
    const report = await client
      .stopStrategy(parentAddress, strategy.id)
      .then(result => (result.ok ? result.data : undefined))
      .catch(() => undefined)
    setBusy(false)
    if (report === undefined) {
      // The exchange stops the strategy before it closes the account, so a failure does not
      // prove nothing happened; the message claims only what is known.
      setPanel({ kind: 'error', message: 'Could not stop the strategy. Try again.' })
      return
    }
    if (report.removed) {
      setRemoved(true)
      return
    }
    setStopped(true)
    setPanel({ kind: 'none' })
    onStopPending?.(strategy.name)
  }

  const keepRunning = () => {
    setPanel({ kind: 'none' })
    stopButtonRef.current?.focus()
  }

  return (
    <>
      <tr className={cn('h-[76px]', showDivider && 'border-b border-border-subtle')}>
        <th
          className="max-w-[300px] overflow-hidden py-xl pl-xl text-left font-regular align-middle"
          scope="row"
        >
          <div className="flex min-w-0 flex-col gap-xs">
            <div className="flex min-w-0 items-center gap-sm">
              <span
                className="block min-w-0 flex-1 truncate font-display text-heading-sm font-bold text-foreground"
                title={strategy.name}
              >
                {strategy.name}
              </span>
              {paused ? (
                <Badge className="h-lg shrink-0 px-xs py-0 text-caption" variant="neutral">
                  Paused
                </Badge>
              ) : null}
              {hasControls ? (
                <span className="flex shrink-0 items-center gap-xs">
                  <Button
                    aria-label={`${paused ? 'Resume' : 'Pause'} ${strategy.name}`}
                    className={controlClasses}
                    disabled={busy !== false}
                    onClick={toggleStatus}
                    size="small"
                    variant="ghost"
                  >
                    {paused ? 'Resume' : 'Pause'}
                  </Button>
                  <Button
                    aria-label={`Stop ${strategy.name}`}
                    className={controlClasses}
                    disabled={busy !== false}
                    onClick={() => setPanel({ kind: 'confirm' })}
                    ref={stopButtonRef}
                    size="small"
                    variant="ghost"
                  >
                    Stop
                  </Button>
                </span>
              ) : null}
            </div>
            {apiUrl === undefined ? null : <RecordLine address={strategy.id} apiUrl={apiUrl} />}
          </div>
        </th>

        <td className="overflow-hidden whitespace-nowrap pl-sm align-middle text-label font-medium">
          {strategy.position ? (
            <span className="flex min-w-0 flex-col gap-xs">
              <span className="flex min-w-0 items-center gap-sm text-foreground">
                <span
                  aria-hidden="true"
                  className={cn(
                    'size-sm shrink-0 rounded-pill border-[0.5px]',
                    positionDotClasses[strategy.position.side]
                  )}
                />
                <span className="truncate font-mono">{strategy.position.label}</span>
              </span>
              <span className="font-mono text-muted-foreground">
                {money.usd(strategy.position.valueCents)}
              </span>
            </span>
          ) : (
            <span className="flex items-center gap-sm text-muted-foreground">
              <span aria-hidden="true" className="h-px w-sm shrink-0 bg-muted-foreground" />
              No Position
            </span>
          )}
        </td>

        <td className="overflow-hidden whitespace-nowrap text-center align-middle font-mono text-body font-medium text-foreground">
          {strategy.trades}
        </td>
        <td
          className={cn(
            'overflow-hidden whitespace-nowrap pl-sm text-right align-middle font-mono text-body font-medium',
            pnlIsPositive ? 'text-positive' : 'text-negative'
          )}
        >
          {money.usd(strategy.totalPnlCents, { signDisplay: 'always' })}
        </td>
        <td className="overflow-hidden pr-xl text-right align-middle">
          <MiniForwardIcon className="ml-auto size-[18px] text-muted-foreground" />
        </td>
      </tr>

      {panel.kind === 'none' ? null : (
        <tr>
          <td className="px-xl pb-xl align-top text-label" colSpan={5}>
            {panel.kind === 'confirm' ? (
              <div className="flex flex-col gap-sm">
                <p className="text-muted-foreground">
                  {`Stop ${strategy.name}? Its open orders are cancelled and it places no more. Its position is closed at the market when that is possible; otherwise it settles with the round. Its money then returns to your paper balance. Rounds already sealed on Arc stay there.`}
                </p>
                <div className="flex flex-wrap items-center gap-sm">
                  <Button
                    disabled={busy === 'stop'}
                    onClick={confirmStop}
                    size="small"
                    variant="danger"
                  >
                    {busy === 'stop' ? 'Stopping…' : 'Stop strategy'}
                  </Button>
                  <Button
                    disabled={busy === 'stop'}
                    onClick={keepRunning}
                    size="small"
                    variant="secondary"
                  >
                    Keep it running
                  </Button>
                </div>
              </div>
            ) : (
              <p className="text-negative" role="alert">
                {panel.message}
              </p>
            )}
          </td>
        </tr>
      )}
    </>
  )
}
