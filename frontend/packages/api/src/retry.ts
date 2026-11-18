export interface RetryPolicy {
  /** Total attempts including the first. */
  attempts: number
  /** Delay before the first retry; doubles each time, then is jittered. */
  baseDelayMs: number
  /** Upper bound for a single delay. */
  maxDelayMs: number
  /** Wall-clock budget for the whole request including retries. */
  budgetMs: number
}

export const DEFAULT_RETRY_POLICY: RetryPolicy = {
  attempts: 3,
  baseDelayMs: 250,
  maxDelayMs: 2_000,
  budgetMs: 8_000,
}

/**
 * Exponential backoff with full jitter, so many clients retrying after the same outage
 * do not hit the server in lockstep.
 */
export function retryDelayMs(
  attemptIndex: number,
  policy: RetryPolicy,
  random: () => number = Math.random
): number {
  const exponential = Math.min(policy.maxDelayMs, policy.baseDelayMs * 2 ** attemptIndex)

  return Math.floor(random() * exponential)
}

export function defaultSleep(ms: number): Promise<void> {
  return new Promise(resolve => {
    setTimeout(resolve, ms)
  })
}
