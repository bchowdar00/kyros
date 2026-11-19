'use client'

import type { MarketTrading } from './exchange/buy'
import type { StrategyDeploy } from './exchange/deploy'

import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useEffect, useRef, type ReactNode } from 'react'

import { BuyWidget } from './buy-widget'
import { CompiledStrategyWidget } from './compiled-strategy-widget'
import { type ExchangeApi, LivePublicStrategyWidget } from './live-public-strategy-widget'
import { PublicStrategyWidget } from './public-strategy-widget'
import type { MarketOutcome } from './types'
import { useTradeTicket } from './use-trade-ticket'
import { CardExpansionAnimation } from '@kyros/ui/lib/card-expansion-animation'

interface ChatPanelInteractionProps {
  trading?: MarketTrading | undefined
  deploy?: StrategyDeploy | undefined
  /** Where the strategy widgets refetch and deploy; absent for static fixtures. */
  exchangeApi?: ExchangeApi | undefined
  children: ReactNode
  composer: ReactNode
  /** Pinned above the composer and scrolls with it, e.g. Kyros's open question. */
  pinned?: ReactNode
  marketKey: string
  outcomes: readonly MarketOutcome[]
  scopeLabel: string
}

export function ChatPanelInteraction({
  trading,
  deploy,
  exchangeApi,
  children,
  composer,
  pinned,
  marketKey,
  outcomes,
  scopeLabel,
}: ChatPanelInteractionProps) {
  const { selection, ticket } = useTradeTicket(marketKey)
  const reducedMotion = useReducedMotion()
  const active = selection !== null
  // Live quote updates must not continuously retarget the submit button's layout.
  const liveBuy = trading !== undefined && selection?.kind === 'buy'
  const animation = new CardExpansionAnimation(reducedMotion)
  const transition = animation.transition
  const returnFocusId = useRef<string | undefined>(undefined)

  function close() {
    returnFocusId.current = selection?.triggerId
    ticket.close()
  }

  function restoreFocus() {
    // The card is inert until the closing state commits and the widget exits.
    if (ticket.getSnapshot() === null && returnFocusId.current) {
      document.getElementById(returnFocusId.current)?.focus({ preventScroll: true })
      returnFocusId.current = undefined
    }
  }

  useEffect(() => {
    if (!selection) return undefined
    const triggerId = selection.triggerId
    function dismissOnEscape(event: KeyboardEvent) {
      if (event.key !== 'Escape') return
      event.preventDefault()
      returnFocusId.current = triggerId
      ticket.close()
    }
    document.addEventListener('keydown', dismissOnEscape)
    return () => document.removeEventListener('keydown', dismissOnEscape)
  }, [selection, ticket])

  return (
    <>
      <div
        className="flex min-h-0 flex-1 flex-col"
        inert={active}
        aria-hidden={active || undefined}
      >
        {children}
      </div>
      <AnimatePresence>
        {active ? (
          <motion.button
            key="scrim"
            type="button"
            aria-label={
              selection?.kind === 'strategy'
                ? 'Close public strategy'
                : selection?.kind === 'compiled'
                  ? 'Close deploy screen'
                  : 'Close buy widget'
            }
            tabIndex={-1}
            className="absolute inset-0 z-10 cursor-default rounded-lg bg-background/70"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0, transition }}
            transition={transition}
            onClick={close}
          />
        ) : null}
      </AnimatePresence>
      <section
        aria-label="Strategy composer region"
        className="relative z-20 -mx-md shrink-0 pb-sm"
      >
        <motion.div
          layout={!liveBuy}
          transition={transition}
          className="overflow-clip rounded-composer border border-border-subtle bg-background"
        >
          <motion.div
            inert={active}
            aria-hidden={active || undefined}
            animate={active ? animation.collapsed : animation.expanded}
            transition={transition}
            className="overflow-clip"
          >
            {pinned}
            {composer}
          </motion.div>
          <AnimatePresence initial={false} onExitComplete={restoreFocus}>
            {selection ? (
              <motion.div
                key="market-widget"
                initial={animation.collapsed}
                animate={animation.expanded}
                exit={{ ...animation.collapsed, transition }}
                transition={transition}
                className="overflow-clip"
              >
                {selection.kind === 'buy' ? (
                  <BuyWidget
                    trading={trading}
                    key="buy"
                    amount={selection.amount}
                    onAmountChange={ticket.setAmount}
                    outcomes={outcomes}
                    selectedSide={selection.side}
                    onSelect={ticket.select}
                    onClose={close}
                  />
                ) : selection.kind === 'compiled' ? (
                  <CompiledStrategyWidget
                    key={selection.build.id}
                    build={selection.build}
                    brief={selection.brief}
                    scopeLabel={scopeLabel}
                    exchange={exchangeApi}
                    onClose={close}
                  />
                ) : exchangeApi ? (
                  <LivePublicStrategyWidget
                    key={`${selection.strategy.id}-${selection.inverse}`}
                    api={exchangeApi}
                    deploy={deploy}
                    strategy={selection.strategy}
                    inverse={selection.inverse}
                    scopeLabel={scopeLabel}
                    onClose={close}
                  />
                ) : (
                  <PublicStrategyWidget
                    key={`${selection.strategy.id}-${selection.inverse}`}
                    strategy={selection.strategy}
                    inverse={selection.inverse}
                    scopeLabel={scopeLabel}
                    deploy={deploy}
                    onClose={close}
                  />
                )}
              </motion.div>
            ) : null}
          </AnimatePresence>
        </motion.div>
      </section>
    </>
  )
}
