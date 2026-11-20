import { createRustExchangeClient } from '@kyros/api'
import { create } from 'zustand'

const STORAGE_KEY = 'kyros:exchange-identity'
const GUEST_ALPHABET = 'abcdefghijklmnopqrstuvwxyz0123456789'
/** Six characters of 36 keep collisions unlikely; the exchange only needs a non-empty name. */
const GUEST_SUFFIX_LENGTH = 6

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
  isVerifying: boolean
  error: string | null
  /**
   * Reads local storage and confirms the exchange still knows the account (restarts wipe it);
   * a second call while one is in flight is a no-op.
   */
  verify: (apiUrl: string) => Promise<void>
  /** Opens a paper account under a random guest name; a second call while one is in flight is a no-op. */
  /** Opens a new paper account, under `name` when one is given and not blank. */
  createGuest: (apiUrl: string, name?: string) => Promise<void>
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

/** The longest name a visitor may give an account; longer is cut. */
export const LONGEST_NAME = 24

function guestUsername(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(GUEST_SUFFIX_LENGTH))
  return `guest-${Array.from(bytes, byte => GUEST_ALPHABET[byte % GUEST_ALPHABET.length]).join('')}`
}

const client = (apiUrl: string) =>
  createRustExchangeClient({ baseUrl: apiUrl, retry: { attempts: 1 }, timeoutMs: 5_000 })

/** One session store: the demo account is the whole login (AGENTS.md §7, session state). */
export const useIdentityStore = create<IdentityState>((set, get) => ({
  identity: null,
  balanceTenths: null,
  verified: false,
  isCreating: false,
  isVerifying: false,
  error: null,

  verify: async apiUrl => {
    if (get().isVerifying) return
    // Storage that cannot be read (disabled, private mode) says nothing about the account
    // this session already holds, so the held one is the one to check.
    const current = () => readStored() ?? get().identity
    const subject = current()
    if (subject === null) {
      set({ identity: null, verified: true })
      return
    }
    set({ isVerifying: true })
    const account = await client(apiUrl).getAccount(subject.address)
    // "New paper account" (here or in another tab) may have replaced the account while its
    // check was on the way; an answer about the old one says nothing about the new one.
    if (current()?.address !== subject.address) {
      set({ isVerifying: false })
      return get().verify(apiUrl)
    }
    if (account.ok) {
      set({
        identity: subject,
        balanceTenths: account.data.usdc_balance,
        verified: true,
        isVerifying: false,
      })
      return
    }
    // A 404 means the exchange restarted; a network failure keeps the identity for retry.
    if (account.error.status === 404) {
      writeStored(null)
      set({ identity: null, balanceTenths: null, verified: true, isVerifying: false })
      return
    }
    set({ identity: subject, verified: true, isVerifying: false })
  },

  createGuest: async (apiUrl, name) => {
    if (get().isCreating) return
    set({ isCreating: true, error: null })
    const chosen = (name ?? '').trim().slice(0, LONGEST_NAME)
    const created = await client(apiUrl).createAccount(chosen === '' ? guestUsername() : chosen)
    if (!created.ok) {
      set({ isCreating: false, error: 'Could not reach the exchange.' })
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
