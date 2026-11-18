'use client'

import { Button } from '@tiny/ui/components/button'
import { type FormEvent, useId, useState } from 'react'

import { useIdentityStore } from './identity-store'

/**
 * The exchange's whole login: pick a name, receive a demo account with $1,000. Rendered
 * where trading would be until an address exists; the chart and book stay visible around it.
 */
export function NameGate({ apiUrl }: { apiUrl: string }) {
  const inputId = useId()
  const [name, setName] = useState('')
  const verified = useIdentityStore(state => state.verified)
  const isCreating = useIdentityStore(state => state.isCreating)
  const error = useIdentityStore(state => state.error)
  const createAccount = useIdentityStore(state => state.createAccount)
  const canSubmit = name.trim().length > 0 && !isCreating

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (canSubmit) void createAccount(apiUrl, name)
  }

  if (!verified) {
    return (
      <output className="block py-lg text-label text-muted-foreground">
        Checking your exchange account…
      </output>
    )
  }

  return (
    <form
      aria-label="Create a trading account"
      className="flex w-full flex-col gap-xl rounded-md border border-border-subtle bg-surface-raised p-xl"
      onSubmit={handleSubmit}
    >
      <div className="flex flex-col gap-sm">
        <strong className="text-body">Trade on the local exchange</strong>
        <p className="text-label text-muted-foreground">
          Pick a name and you get a demo account with $1,000 to trade every series.
        </p>
      </div>
      <label className="flex flex-col gap-sm text-label" htmlFor={inputId}>
        Your name
        <input
          autoComplete="username"
          className="min-h-touch rounded-xs border border-border-subtle bg-surface px-md text-body text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
          id={inputId}
          maxLength={32}
          onChange={event => setName(event.target.value)}
          value={name}
        />
      </label>
      {error ? (
        <p className="text-label text-negative" role="alert">
          {error}
        </p>
      ) : null}
      <Button className="min-h-touch w-full" disabled={!canSubmit} type="submit">
        {isCreating ? 'Creating account…' : 'Start trading'}
      </Button>
    </form>
  )
}
