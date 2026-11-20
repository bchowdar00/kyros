export interface KeptLoaderOptions {
  /** How long a failed read stands before the source is asked again. */
  failedForMs: number
  /** How long a good value is served without reading again. */
  freshForMs: number
  now?: () => number
  /**
   * The oldest a kept value may be and still be shown. Past it a caller waits for the new
   * read and gets nothing if that fails. Unset, a kept value is shown however old it is.
   */
  staleForMs?: number
}

/**
 * Wraps a slow read of someone else's service for a page many visitors open. A good value is
 * fresh for a while; after that a caller still gets it at once while a new read runs behind
 * it, and a read that fails never replaces a good value. Callers share a running read. Only a
 * caller who arrives before any good read, or after the kept one has grown too old to show,
 * waits, and gets nothing if that read fails.
 */
export function createKeptLoader<T>(
  read: () => Promise<T>,
  options: KeptLoaderOptions
): () => Promise<T | undefined> {
  const now = options.now ?? Date.now
  let good: { at: number; value: T } | undefined
  let failedAt: number | undefined
  let reading: Promise<T | undefined> | undefined

  function refresh(): Promise<T | undefined> {
    reading ??= read()
      .then(value => {
        good = { at: now(), value }
        failedAt = undefined
      })
      .catch(() => {
        failedAt = now()
      })
      .then(() => {
        reading = undefined
        return good?.value
      })
    return reading
  }

  /** The kept value, unless there is none or it is too old to show. */
  function shown(): { at: number; value: T } | undefined {
    const tooOld =
      good !== undefined &&
      options.staleForMs !== undefined &&
      now() - good.at >= options.staleForMs
    return tooOld ? undefined : good
  }

  return () => {
    const failedLately = failedAt !== undefined && now() - failedAt < options.failedForMs
    const kept = shown()
    if (kept !== undefined) {
      if (now() - kept.at >= options.freshForMs && !failedLately) void refresh()
      return Promise.resolve(kept.value)
    }
    return failedLately ? Promise.resolve(undefined) : refresh().then(() => shown()?.value)
  }
}
