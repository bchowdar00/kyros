/**
 * The work's value if it arrives within `ms`, otherwise nothing. The work is not stopped: it
 * goes on, so whoever asks next may find it done. For a part of a page that is worth having
 * and not worth waiting for.
 */
export function withinMs<T>(work: Promise<T>, ms: number): Promise<T | undefined> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const limit = new Promise<undefined>(resolve => {
    timer = setTimeout(resolve, ms, undefined)
  })
  return Promise.race([work, limit]).finally(() => clearTimeout(timer))
}
