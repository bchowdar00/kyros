import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { withinMs } from './within-ms'

describe('withinMs', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => vi.useRealTimers())

  it('gives the value when the work finishes in time', async () => {
    const answer = withinMs(Promise.resolve('record'), 1500)

    await expect(answer).resolves.toBe('record')
    // Nothing is left ticking once the work is in.
    expect(vi.getTimerCount()).toBe(0)
  })

  it('gives nothing when the work is still running at the limit', async () => {
    // Work that never finishes on its own: the limit is all that can answer.
    const work = new Promise<string>(() => undefined)
    const answer = withinMs(work, 1500)

    await vi.advanceTimersByTimeAsync(1500)
    await expect(answer).resolves.toBeUndefined()
  })
})
