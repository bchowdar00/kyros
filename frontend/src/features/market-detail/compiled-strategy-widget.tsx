'use client'

import { Button } from '@kyros/ui/components/button'
import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { cn } from '@kyros/ui/lib/cn'
import { useId, useState } from 'react'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { deriveEnvelope } from '@/mastra/capabilities/strategy/envelope'
import { useIdentityStore } from '../identity/identity-store'
import {
  type Bankroll,
  applyBankroll,
  bankrollInputs,
  dollarsToTenths,
  initialBankroll,
} from './bankroll'
import type { ExchangeApi } from './live-public-strategy-widget'
import { AllocationField, MoneyField, ScopeField } from './public-strategy-settings'
import type { BuildStatus } from './strategy-build'
import { StrategyLogicTabs } from './strategy-logic-tabs'
import { useStrategyDeploy } from './use-strategy-deploy'
import { WidgetBackButton } from './widget-back-button'

interface CompiledStrategyWidgetProps {
  build: BuildStatus
  brief: StrategyBrief
  scopeLabel: string
  /** Where the deploy posts; absent for static fixtures, which keeps the button inert. */
  exchange?: ExchangeApi | undefined
  onClose: () => void
}

/**
 * The deploy screen for a strategy Kyros just compiled: the same surface Run opens for a
 * public strategy, with the account's own tile and colour where a creator's photo would
 * be. The allocation is the one figure everyone sets; the rest of the bankroll Kyros
 * proposed sits under a disclosure, and the summary line always says what will run.
 */
export function CompiledStrategyWidget({
  build,
  brief,
  scopeLabel,
  exchange,
  onClose,
}: CompiledStrategyWidgetProps) {
  const identity = useIdentityStore(state => state.identity)
  const [bankroll, setBankroll] = useState<Bankroll>(() => initialBankroll(brief))
  const [adjusting, setAdjusting] = useState(false)
  const { ready, status: deployStatus, deploy } = useStrategyDeploy(exchange)
  const bankrollId = useId()
  const name = build.result?.summary?.name ?? 'Your strategy'
  const readback = build.result?.readback ?? ''
  const document = build.result?.strategy
  const derived = deriveEnvelope(applyBankroll(brief, bankroll))
  const money = derived.ok ? derived.money : undefined
  const inputs = bankrollInputs(brief, bankroll)
  const set = (field: keyof Bankroll) => (value: string) =>
    setBankroll(current => ({ ...current, [field]: value }))
  const canDeploy = ready && derived.ok && document !== undefined

  async function submit() {
    if (!derived.ok || document === undefined) return
    const deployed = await deploy({
      allocation_tenths: dollarsToTenths(bankroll.allocation),
      name,
      strategy: document,
      envelope: derived.envelope,
    })
    if (deployed) onClose()
  }

  return (
    <dialog
      open
      aria-label="Deploy strategy"
      className="relative m-0 w-full max-w-none max-h-widget-viewport overflow-y-auto overscroll-contain border-0 bg-background/50 p-0 pb-xl text-foreground"
    >
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-0 overflow-clip bg-surface"
      >
        <div
          className="absolute left-1/2 top-0 size-public-strategy-glow -translate-x-1/2 -translate-y-1/2 rounded-pill opacity-15 blur-public-strategy-glow"
          style={{ backgroundColor: identity?.color }}
        />
      </div>
      <div className="relative">
        <div className="flex flex-col gap-xl p-xl">
          <div className="flex items-center justify-between">
            <WidgetBackButton onClose={onClose} />
            <span className="text-label text-muted-foreground">Your strategy</span>
          </div>
          <div className="flex items-center gap-md">
            <span
              aria-hidden="true"
              className="inline-flex size-avatar-lg shrink-0 items-center justify-center rounded-pill border border-highlight text-heading-sm"
              style={{ backgroundColor: identity?.color }}
            >
              {identity?.emoji}
            </span>
            <div className="min-w-0">
              <h2 className="text-heading-md font-regular">{name}</h2>
              <p className="mt-copy text-label font-medium text-muted-foreground">
                By {identity?.username ?? 'you'}
              </p>
            </div>
          </div>
        </div>
        <div className="mx-xl border-t border-highlight pt-xl">
          <StrategyLogicTabs logic={readback} />
        </div>
        <div className="px-xl pt-xl">
          <AllocationField value={inputs.allocation} onChange={set('allocation')} />
          {money ? (
            <p className="pb-md text-label text-muted-foreground" aria-live="polite">
              Sees <span className="font-mono text-foreground">{money.roundBudget}</span> per round,
              up to <span className="font-mono text-foreground">{money.tradeBudget}</span> per
              order, stops after losing{' '}
              <span className="font-mono text-foreground">{money.stopAfterLoss}</span>.
            </p>
          ) : (
            <p role="alert" className="pb-md text-label text-foreground">
              {derived.ok ? '' : derived.problem}
            </p>
          )}
          <button
            type="button"
            aria-expanded={adjusting}
            aria-controls={bankrollId}
            onClick={() => setAdjusting(current => !current)}
            className="flex min-h-touch w-full items-center justify-between gap-md py-md text-left text-label font-medium text-muted-foreground focus-visible:outline-2 focus-visible:outline-focus-ring"
          >
            Adjust bankroll
            <NanoChevronIcon
              className={cn('text-foreground motion-safe:transition-transform', {
                'rotate-180': adjusting,
              })}
            />
          </button>
          <div id={bankrollId} hidden={!adjusting} className="border-t border-highlight">
            <MoneyField
              compact
              label="Per round"
              name="Per round budget in USD"
              value={inputs.roundBudget}
              onChange={set('roundBudget')}
            />
            <MoneyField
              compact
              label="Per order"
              name="Per order budget in USD"
              value={inputs.tradeBudget}
              onChange={set('tradeBudget')}
            />
            <MoneyField
              compact
              label="Stop after losing"
              name="Stop after losing in USD"
              value={inputs.stopAfterLoss}
              onChange={set('stopAfterLoss')}
            />
          </div>
          <ScopeField scopeLabel={scopeLabel} fixed />
        </div>
        <div className="flex flex-col gap-sm px-xl pt-xl">
          <Button className="min-h-touch w-full text-body" disabled={!canDeploy} onClick={submit}>
            {deployStatus.state === 'pending' ? 'Deploying…' : 'Deploy strategy'}
          </Button>
          {deployStatus.state === 'failed' ? (
            <p role="alert" className="text-center text-caption text-foreground">
              {deployStatus.message}
            </p>
          ) : (
            <p className="text-center text-caption text-muted-foreground">
              Funds the strategy from your balance and starts it on this market.
            </p>
          )}
        </div>
      </div>
    </dialog>
  )
}
