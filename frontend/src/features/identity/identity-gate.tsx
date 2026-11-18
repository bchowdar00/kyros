'use client'

import { TinyMarkIcon } from '@tiny/ui/icons/tiny-mark-icon'
import { type ReactNode, useEffect } from 'react'

import { useIdentityStore } from './identity-store'
import { NameGate } from './name-gate'

/**
 * The whole app waits behind a name, as the old UI did: nothing renders until the browser
 * holds an address the exchange still knows. Without an exchange there is nothing to gate.
 */
export function IdentityGate({ apiUrl, children }: { apiUrl?: string; children: ReactNode }) {
  const identity = useIdentityStore(state => state.identity)
  const verified = useIdentityStore(state => state.verified)
  const verify = useIdentityStore(state => state.verify)

  useEffect(() => {
    // Session bootstrap: read the stored account and confirm the exchange still has it.
    if (apiUrl !== undefined) void verify(apiUrl)
  }, [apiUrl, verify])

  if (apiUrl === undefined || identity !== null) {
    return children
  }

  return (
    <main className="flex min-h-dvh flex-col items-center justify-center gap-xl px-xl text-center">
      <TinyMarkIcon aria-hidden className="size-2xl text-foreground" />
      <h1 className="text-2xl font-regular text-foreground">Welcome to Tiny</h1>
      {verified ? (
        <div className="w-[26rem] max-w-full text-left">
          <NameGate apiUrl={apiUrl} />
        </div>
      ) : (
        <output className="text-label text-muted-foreground">Checking your account…</output>
      )}
    </main>
  )
}
