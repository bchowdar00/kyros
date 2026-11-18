import { createRustExchangeClient } from '@tiny/api'
import { create } from 'zustand'

const STORAGE_KEY = 'tiny:exchange-identity'

export interface ExchangeIdentity {
  address: string
  username: string
  color: string
  emoji: string
}

interface IdentityState {
  identity: ExchangeIdentity | null
  /** Tenths of a cent, refreshed from the exchange on verify and by the market socket. */
  balanceTenths: number | null
  /** False until the stored identity has been read and checked against the exchange. */
  verified: boolean
  isCreating: boolean
  error: string | null
  /** Reads local storage and confirms the exchange still knows the account (restarts wipe it). */
  verify: (apiUrl: string) => Promise<void>
  createAccount: (apiUrl: string, username: string) => Promise<void>
  setBalance: (balanceTenths: number) => void
  logout: () => void
}

function readStored(): ExchangeIdentity | null {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY)
    if (raw === null) return null
    const parsed: unknown = JSON.parse(raw)
    if (
      typeof parsed === 'object' &&
      parsed !== null &&
      'address' in parsed &&
      typeof parsed.address === 'string'
    ) {
      return parsed as ExchangeIdentity
    }
    return null
  } catch {
    // Storage may be disabled; identity is then session-scoped.
    return null
  }
}

function writeStored(identity: ExchangeIdentity | null): void {
  try {
    if (identity === null) window.localStorage.removeItem(STORAGE_KEY)
    else window.localStorage.setItem(STORAGE_KEY, JSON.stringify(identity))
  } catch {
    // Storage may be disabled; identity is then session-scoped.
  }
}

const client = (apiUrl: string) =>
  createRustExchangeClient({ baseUrl: apiUrl, retry: { attempts: 1 }, timeoutMs: 5_000 })

/** One session store: the demo account is the whole login (AGENTS.md §7, session state). */
export const useIdentityStore = create<IdentityState>((set, get) => ({
  identity: null,
  balanceTenths: null,
  verified: false,
  isCreating: false,
  error: null,

  verify: async apiUrl => {
    const stored = readStored()
    if (stored === null) {
      set({ identity: null, verified: true })
      return
    }
    const account = await client(apiUrl).getAccount(stored.address)
    if (account.ok) {
      set({ identity: stored, balanceTenths: account.data.usdc_balance, verified: true })
      return
    }
    // A 404 means the exchange restarted; a network failure keeps the identity for retry.
    if (account.error.status === 404) {
      writeStored(null)
      set({ identity: null, balanceTenths: null, verified: true })
      return
    }
    set({ identity: stored, verified: true })
  },

  createAccount: async (apiUrl, rawName) => {
    const username = rawName.trim()
    if (username.length === 0) {
      set({ error: 'Enter a name to trade under.' })
      return
    }
    set({ isCreating: true, error: null })
    const created = await client(apiUrl).createAccount(username)
    if (!created.ok) {
      set({ isCreating: false, error: 'Could not reach the exchange. Is the server running?' })
      return
    }
    const identity: ExchangeIdentity = {
      address: created.data.address,
      username: created.data.username,
      color: created.data.color,
      emoji: created.data.emoji,
    }
    writeStored(identity)
    set({ identity, balanceTenths: created.data.usdc_balance, isCreating: false, verified: true })
  },

  setBalance: balanceTenths => {
    if (get().balanceTenths !== balanceTenths) set({ balanceTenths })
  },

  logout: () => {
    writeStored(null)
    set({ identity: null, balanceTenths: null, error: null })
  },
}))
