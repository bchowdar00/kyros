'use client'

import { Button } from '@kyros/ui/components/button'
import { type ReactNode, useEffect } from 'react'

import { Wordmark } from '@/components/wordmark/wordmark'

import { useIdentityStore } from './identity-store'

/** How often a held account is re-checked against the exchange. */
const REVERIFY_INTERVAL_MS = 60_000

/**
 * The whole app waits for a paper account: nothing renders until the browser holds an
 * address the exchange still knows, and a missing one is opened silently as a guest.
 * Without an exchange there is nothing to gate.
 */
export function IdentityGate({ apiUrl, children }: { apiUrl?: string; children: ReactNode }) {
  const identity = useIdentityStore(state => state.identity)
  const verified = useIdentityStore(state => state.verified)
  const error = useIdentityStore(state => state.error)
  const verify = useIdentityStore(state => state.verify)
  const createGuest = useIdentityStore(state => state.createGuest)

  useEffect(() => {
    // Session bootstrap: confirm the stored account, then open a guest when there is none
    // (first visit, or after "New paper account"). A failure waits for "Try again".
    if (apiUrl === undefined) return
    if (!verified) void verify(apiUrl)
    else if (identity === null && error === null) void createGuest(apiUrl)
  }, [apiUrl, verified, identity, error, verify, createGuest])

  const held = identity !== null
  useEffect(() => {
    // An exchange restart forgets every account; re-checking the held one notices it without a
    // reload. A 404 clears the identity and the effect above opens a fresh guest.
    if (apiUrl === undefined || !held) return
    const reverify = () => void verify(apiUrl)
    const timer = setInterval(reverify, REVERIFY_INTERVAL_MS)
    window.addEventListener('focus', reverify)
    return () => {
      clearInterval(timer)
      window.removeEventListener('focus', reverify)
    }
  }, [apiUrl, held, verify])

  if (apiUrl === undefined || identity !== null) {
    return children
  }

  return (
    <main className="flex min-h-dvh flex-col items-center justify-center gap-xl px-xl text-center">
      <Wordmark aria-hidden="true" />
      <h1 className="font-display text-display-md font-bold tracking-display text-foreground">
        Welcome to Kyros
      </h1>
      {error === null ? (
        <output className="text-label text-muted-foreground">
          {verified ? 'Setting up your paper account…' : 'Checking your account…'}
        </output>
      ) : (
        <div className="flex flex-col items-center gap-md">
          <p className="text-label text-negative" role="alert">
            {error}
          </p>
          <Button className="min-h-touch" onClick={() => void createGuest(apiUrl)}>
            Try again
          </Button>
        </div>
      )}
    </main>
  )
}
