import { describe, expect, it } from 'vitest'
import { createBitcoinDemo, demoPriceCents } from './bitcoin-demo'

describe('Bitcoin demos', () => {
  it('replays exactly and stops advancing while paused', () => {
    const demo = createBitcoinDemo('rally')
    const initial = demo.getSnapshot()
    for (let tick = 0; tick < 16; tick += 1) demo.advance()
    expect(demoPriceCents('rally', 16) - demoPriceCents('steady', 16)).toBe(24_000)
    const advanced = demo.getSnapshot()
    expect(advanced.points.length).toBe(initial.points.length)
    expect(advanced.now - initial.now).toBe(4_000)
    demo.togglePause()
    const paused = demo.getSnapshot()
    demo.advance()
    expect(demo.getSnapshot()).toBe(paused)
    demo.replay()
    expect(demo.getSnapshot()).toBe(initial)
  })

  it('does not manufacture observations during a disconnection', () => {
    const demo = createBitcoinDemo('disconnect')
    for (let tick = 0; tick < 15; tick += 1) demo.advance()
    const before = demo.getSnapshot().points
    for (let tick = 0; tick < 20; tick += 1) demo.advance()
    expect(demo.getSnapshot().disconnected).toBe(true)
    expect(demo.getSnapshot().points).toBe(before)
    for (let tick = 0; tick < 5; tick += 1) demo.advance()
    expect(demo.getSnapshot().disconnected).toBe(false)
    expect(demo.getSnapshot().points.at(-1)?.time).toBeGreaterThan(before.at(-1)?.time ?? 0)
  })
  it('continues its tape when the next market replaces the target and deadline', () => {
    const demo = createBitcoinDemo('rollover')
    for (let tick = 0; tick < 31; tick += 1) demo.advance()
    const previous = demo.getSnapshot()
    demo.advance()
    const next = demo.getSnapshot()
    expect(next.marketEndsAt - previous.marketEndsAt).toBe(300_000)
    expect(next.targetPriceCents).toBe(demoPriceCents('rollover', 32))
    expect(next.points.slice(0, -1)).toEqual(previous.points.slice(1))
    expect(next.now - previous.now).toBe(250)
    demo.advance()
    expect(demo.getSnapshot().points.at(-1)?.time).toBe(next.now + 250)
  })
})
