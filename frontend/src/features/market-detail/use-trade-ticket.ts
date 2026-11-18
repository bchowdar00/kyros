'use client'

import { useSyncExternalStore } from 'react'

import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import type { BuildStatus } from './strategy-build'
import type { MarketSide, PublicStrategy } from './types'

interface BuySelection {
  kind: 'buy'
  amount: string
  side: MarketSide
  triggerId: string
}

interface StrategySelection {
  kind: 'strategy'
  strategy: PublicStrategy
  inverse: boolean
  triggerId: string
}

/** A strategy Teeny just compiled, on its way to the deploy screen. */
export interface CompiledSelection {
  kind: 'compiled'
  build: BuildStatus
  brief: StrategyBrief
  triggerId: string
}

type TradeSelection = BuySelection | StrategySelection | CompiledSelection

function createTradeTicket(onIdle: () => void) {
  let selection: TradeSelection | null = null
  const listeners = new Set<() => void>()

  function publish(next: TradeSelection | null) {
    selection = next
    for (const listener of listeners) listener()
  }

  return {
    getSnapshot: () => selection,
    subscribe(listener: () => void) {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
        // React can resubscribe during the same commit; release only after it settles.
        queueMicrotask(() => {
          if (listeners.size === 0) onIdle()
        })
      }
    },
    open(side: MarketSide, triggerId: string) {
      publish({
        kind: 'buy',
        amount: selection?.kind === 'buy' ? selection.amount : '',
        side,
        triggerId,
      })
    },
    openStrategy(strategy: PublicStrategy, inverse: boolean, triggerId: string) {
      publish({ kind: 'strategy', strategy, inverse, triggerId })
    },
    openCompiled(build: BuildStatus, brief: StrategyBrief, triggerId: string) {
      publish({ kind: 'compiled', build, brief, triggerId })
    },
    select(side: MarketSide) {
      if (selection?.kind === 'buy') publish({ ...selection, side })
    },
    setAmount(amount: string) {
      if (selection?.kind === 'buy') publish({ ...selection, amount })
    },
    close() {
      publish(null)
    },
  }
}

const tickets = new Map<string, ReturnType<typeof createTradeTicket>>()
const closedSnapshot = () => null

export function useTradeTicket(marketKey: string) {
  let ticket = typeof window === 'undefined' ? undefined : tickets.get(marketKey)
  if (!ticket) {
    ticket = createTradeTicket(() => {
      if (tickets.get(marketKey) === ticket) tickets.delete(marketKey)
    })
    if (typeof window !== 'undefined') tickets.set(marketKey, ticket)
  }
  const selection = useSyncExternalStore(ticket.subscribe, ticket.getSnapshot, closedSnapshot)
  return { selection, ticket }
}
