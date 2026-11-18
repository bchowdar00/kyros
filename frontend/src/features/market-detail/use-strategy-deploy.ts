'use client'

import { createRustExchangeClient } from '@tiny/api'
import type { RustCreateStrategyRequest } from '@tiny/domain'
import { useState } from 'react'

import { useIdentityStore } from '../identity/identity-store'
import type { ExchangeApi } from './live-public-strategy-widget'

/** Compiling again plus starting the runtime; well under this on a healthy exchange. */
const DEPLOY_TIMEOUT_MS = 10_000

export type DeployStatus =
  | { state: 'idle' }
  | { state: 'pending' }
  | { state: 'failed'; message: string }

/**
 * One deploy from a widget: the user's account funds a sub-account on `exchange` and the
 * runtime starts it. `ready` is false without an account or an exchange (static fixtures),
 * which keeps the button inert rather than pretending.
 */
export function useStrategyDeploy(exchange: ExchangeApi | undefined) {
  const identity = useIdentityStore(state => state.identity)
  const [status, setStatus] = useState<DeployStatus>({ state: 'idle' })
  const ready = identity !== null && exchange !== undefined && status.state !== 'pending'

  /** Resolves to true once the exchange has the strategy running. */
  async function deploy(input: RustCreateStrategyRequest): Promise<boolean> {
    if (identity === null || exchange === undefined) return false
    setStatus({ state: 'pending' })
    const client = createRustExchangeClient({
      baseUrl: exchange.apiUrl,
      retry: { attempts: 1 },
      timeoutMs: DEPLOY_TIMEOUT_MS,
    })
    const deployed = await client.createStrategy(identity.address, input)
    if (deployed.ok) {
      setStatus({ state: 'idle' })
      return true
    }
    setStatus({ state: 'failed', message: deployed.error.message })
    return false
  }

  return { ready, status, deploy }
}
