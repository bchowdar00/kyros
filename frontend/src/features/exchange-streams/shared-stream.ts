/**
 * One connection per key, shared by every consumer and released a little after the last
 * one leaves. Navigation may briefly have no consumer, and a round rollover must not
 * tear a feed down, so release waits `RELEASE_DELAY_MS`.
 */
export const RELEASE_DELAY_MS = 30_000

export interface SharedStream<Snapshot> {
  getSnapshot(): Snapshot
  /** Ref-counted: the first listener connects, the last one schedules the release. */
  subscribe(listener: () => void): () => void
}

export interface SharedStreamConnection {
  /** Close the connection; called once the last consumer has left and the delay passed. */
  close(): void
}

/**
 * The store behind a shared stream: a snapshot, listeners, and the connect/release cycle.
 * `connect` opens the resource and returns what closes it; `publish` replaces the snapshot
 * and notifies. The store's own methods (send, register) are added by the caller.
 */
export function createSharedStream<Snapshot>(
  initial: Snapshot,
  connect: (publish: (next: Snapshot) => void, current: () => Snapshot) => SharedStreamConnection,
  onRelease: () => void
): SharedStream<Snapshot> & {
  publish(next: Snapshot): void
  /** Whether a connection is open, for stores that send on it. */
  connected(): boolean
} {
  let snapshot = initial
  let connection: SharedStreamConnection | undefined
  let releaseTimer: ReturnType<typeof setTimeout> | undefined
  const listeners = new Set<() => void>()
  const publish = (next: Snapshot) => {
    snapshot = next
    for (const listener of listeners) listener()
  }
  return {
    getSnapshot: () => snapshot,
    publish,
    connected: () => connection !== undefined,
    subscribe(listener) {
      clearTimeout(releaseTimer)
      listeners.add(listener)
      if (connection === undefined) connection = connect(publish, () => snapshot)
      return () => {
        listeners.delete(listener)
        if (listeners.size === 0) {
          releaseTimer = setTimeout(() => {
            connection?.close()
            connection = undefined
            onRelease()
          }, RELEASE_DELAY_MS)
        }
      }
    },
  }
}

/** One live store per key, created on first use and forgotten when it releases itself. */
export function createStreamRegistry<Store>(create: (key: string, release: () => void) => Store) {
  const stores = new Map<string, Store>()
  return (key: string): Store => {
    let store = stores.get(key)
    if (store === undefined) {
      store = create(key, () => stores.delete(key))
      stores.set(key, store)
    }
    return store
  }
}
