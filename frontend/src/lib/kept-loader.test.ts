// @vitest-environment node
import { describe, expect, it, vi } from 'vitest'

import { createKeptLoader } from './kept-loader'

function setup(results: (number | Error)[], staleForMs?: number) {
  let clock = 0
  const read = vi.fn(async () => {
    const next = results.shift()
    if (next === undefined || next instanceof Error) throw next ?? new Error('no more answers')
    return next
  })
  const load = createKeptLoader(read, {
    failedForMs: 5000,
    freshForMs: 30_000,
    now: () => clock,
    ...(staleForMs === undefined ? {} : { staleForMs }),
  })
  return { advance: (ms: number) => (clock += ms), load, read }
}

describe('a kept loader', () => {
  it('reads once and answers from what it kept while that is fresh', async () => {
    const { advance, load, read } = setup([1, 2])

    expect(await load()).toBe(1)
    advance(29_000)
    expect(await load()).toBe(1)
    expect(read).toHaveBeenCalledTimes(1)
  })

  it('answers at once from a stale value and reads again behind it', async () => {
    const { advance, load, read } = setup([1, 2])
    await load()

    advance(31_000)
    expect(await load()).toBe(1)
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2))
    await vi.waitFor(async () => expect(await load()).toBe(2))
  })

  it('keeps the last good value when a later read fails, and waits before asking again', async () => {
    const { advance, load, read } = setup([1, new Error('down'), 3])
    await load()

    advance(31_000)
    expect(await load()).toBe(1)
    await vi.waitFor(() => expect(read).toHaveBeenCalledTimes(2))
    advance(4000)
    expect(await load()).toBe(1)
    expect(read).toHaveBeenCalledTimes(2)

    advance(2000)
    expect(await load()).toBe(1)
    await vi.waitFor(async () => expect(await load()).toBe(3))
  })

  it('has nothing to give when the first read fails, and tries again after the wait', async () => {
    const { advance, load, read } = setup([new Error('down'), 2])

    expect(await load()).toBeUndefined()
    advance(1000)
    expect(await load()).toBeUndefined()
    expect(read).toHaveBeenCalledTimes(1)

    advance(5000)
    expect(await load()).toBe(2)
  })

  it('shares one read between callers who arrive together', async () => {
    const { load, read } = setup([1])

    expect(await Promise.all([load(), load()])).toEqual([1, 1])
    expect(read).toHaveBeenCalledTimes(1)
  })

  it('makes the caller wait for a new read once the kept value is too old to show', async () => {
    const { advance, load, read } = setup([1, 2], 60_000)
    await load()

    advance(61_000)
    expect(await load()).toBe(2)
    expect(read).toHaveBeenCalledTimes(2)
  })

  it('gives nothing, not a value too old to show, when that new read fails', async () => {
    const { advance, load } = setup([1, new Error('down'), 3], 60_000)
    await load()

    advance(61_000)
    expect(await load()).toBeUndefined()
    // Still nothing while the failure stands; the next good read is shown again.
    advance(1000)
    expect(await load()).toBeUndefined()
    advance(5000)
    expect(await load()).toBe(3)
  })
})
