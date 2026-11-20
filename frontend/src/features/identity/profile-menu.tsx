'use client'

import { Button } from '@kyros/ui/components/button'
import { useId, useState } from 'react'

import { type ExchangeIdentity, LONGEST_NAME, useIdentityStore } from './identity-store'

const focusRingClasses =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'

function shortAddress(address: string): string {
  return `${address.slice(0, 6)}…${address.slice(-4)}`
}

/**
 * The account avatar: its colour and emoji come from the exchange, not the user. The menu
 * is a native `popover`, so Escape and outside clicks dismiss it without any script.
 */
export function ProfileMenu({
  apiUrl,
  identity,
}: {
  /** The exchange a new account is opened on; without one the menu offers no name. */
  apiUrl?: string | undefined
  identity: ExchangeIdentity
}) {
  const menuId = useId()
  const logout = useIdentityStore(state => state.logout)
  const createGuest = useIdentityStore(state => state.createGuest)
  const [copied, setCopied] = useState(false)
  const [name, setName] = useState('')

  const copyAddress = async () => {
    try {
      await navigator.clipboard.writeText(identity.address)
      setCopied(true)
    } catch {
      setCopied(false)
    }
  }

  return (
    <>
      <button
        aria-label={`Account menu for ${identity.username}`}
        className={`inline-flex min-h-touch min-w-touch items-center justify-center rounded-pill ${focusRingClasses}`}
        popoverTarget={menuId}
        type="button"
      >
        <span
          aria-hidden="true"
          className="inline-flex size-avatar-sm items-center justify-center rounded-pill text-body"
          style={{ backgroundColor: identity.color }}
        >
          {identity.emoji}
        </span>
      </button>
      <div
        aria-label="Account"
        // Author `display` beats the UA's closed-popover rule, so closed must be spelled out.
        className="fixed inset-auto top-app-nav right-xl m-0 hidden w-64 flex-col gap-md rounded-md border border-border-subtle bg-surface-raised p-lg text-label text-foreground [&:popover-open]:flex"
        id={menuId}
        onToggle={event => {
          if (event.newState === 'closed') setCopied(false)
        }}
        popover="auto"
      >
        <div className="flex items-center gap-md">
          <span
            aria-hidden="true"
            className="inline-flex size-avatar-md items-center justify-center rounded-pill text-heading-sm"
            style={{ backgroundColor: identity.color }}
          >
            {identity.emoji}
          </span>
          <strong className="truncate text-body">{identity.username}</strong>
        </div>
        <button
          className={`flex min-h-touch items-center justify-between rounded-xs font-mono text-muted-foreground hover:text-foreground ${focusRingClasses}`}
          onClick={() => void copyAddress()}
          title={identity.address}
          type="button"
        >
          <span>{shortAddress(identity.address)}</span>
          <span className="text-caption">{copied ? 'Copied' : 'Copy'}</span>
        </button>
        {apiUrl === undefined ? null : (
          <form
            className="flex flex-col gap-sm border-t border-border-subtle pt-md"
            onSubmit={event => {
              event.preventDefault()
              const chosen = name.trim()
              if (chosen === '') return
              void createGuest(apiUrl, chosen)
              setName('')
            }}
          >
            <label className="flex flex-col gap-2xs text-caption text-muted-foreground">
              Your name
              <input
                autoComplete="off"
                className={`min-h-touch rounded-xs border border-border-subtle bg-background px-md text-label text-foreground ${focusRingClasses}`}
                maxLength={LONGEST_NAME}
                onChange={event => setName(event.target.value)}
                value={name}
              />
            </label>
            <Button className="min-h-touch w-full" type="submit" variant="secondary">
              Use this name
            </Button>
            <p className="text-caption text-muted-foreground">
              Starts a new paper account under this name.
            </p>
          </form>
        )}
        <Button className="min-h-touch w-full" onClick={logout} variant="ghost">
          New paper account
        </Button>
      </div>
    </>
  )
}
