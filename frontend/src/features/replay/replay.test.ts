import { describe, expect, it } from 'vitest'

import {
  HOLD_MS,
  PHASE_STEP_MS,
  priceLine,
  type ReplayGame,
  ReplayGameSchema,
  ReplayPlaySchema,
  ReplayPriceSchema,
  positionAt,
  replayDay,
  stateAt,
} from './replay'

const game: ReplayGame = {
  id: 'g1',
  date: '2026-11-19',
  home: { code: 'MEM', name: 'Memphis Grizzlies' },
  away: { code: 'GS', name: 'Golden State Warriors' },
  durationMs: 20_000,
  final: { home: 5, away: 3 },
  plays: [
    { atMs: 0, period: 1, clock: '12:00', home: 0, away: 0, team: null, points: 0 },
    { atMs: 5_000, period: 1, clock: '11:35', home: 2, away: 0, team: 'home', points: 2 },
    { atMs: 12_000, period: 2, clock: '8:20', home: 2, away: 3, team: 'away', points: 3 },
    { atMs: 20_000, period: 3, clock: '4:12', home: 5, away: 3, team: 'home', points: 3 },
  ],
  prices: [
    { atMs: 0, bid: 0.6, ask: 0.62 },
    { atMs: 10_000, bid: 0.66, ask: 0.68 },
  ],
}

describe('positionAt', () => {
  it('wraps back to 0 after a full cycle', () => {
    const cycle = game.durationMs + HOLD_MS
    expect(cycle).toBe(80_000)
    const position = positionAt(cycle, game.durationMs, 0)
    expect(position.atMs).toBe(0)
    expect(position.ended).toBe(false)
    expect(position.startedAtMs).toBe(cycle)
  })

  it('reports ended during the hold after the final play', () => {
    const position = positionAt(25_000, game.durationMs, 0)
    expect(position.atMs).toBe(20_000)
    expect(position.ended).toBe(true)
    expect(position.startedAtMs).toBe(0)
  })

  it('shifts the phase by the index', () => {
    expect(PHASE_STEP_MS).toBe(420_000)
    const zero = positionAt(25_000, game.durationMs, 0)
    const one = positionAt(25_000, game.durationMs, 1)
    expect(one.atMs).toBe(5_000)
    expect(one.atMs).not.toBe(zero.atMs)
  })

  it('stays within the game for a nowMs smaller than the phase', () => {
    const position = positionAt(1_000, game.durationMs, 5)
    expect(position.atMs).toBeGreaterThanOrEqual(0)
    expect(position.atMs).toBeLessThanOrEqual(game.durationMs)
  })
})

/** A play that only carries a score, for the scoring-list cases. */
const scoreAt = (atMs: number, home: number, away: number) => ({
  atMs,
  away,
  clock: '5:00',
  home,
  period: 2,
  points: 0,
  team: null,
})

describe('stateAt', () => {
  it('reads the last play at or before the position', () => {
    const state = stateAt(game, 12_000)
    expect(state.period).toBe(2)
    expect(state.clock).toBe('8:20')
    expect(state.home).toBe(2)
    expect(state.away).toBe(3)
    expect(state.homePercent).toBe(67)
    expect(state.changePercent).toBe(6)
    expect(state.scoring.map(play => play.atMs)).toEqual([12_000, 5_000])
    expect(state.scoring).toHaveLength(2)
  })

  it('starts at 12:00, 0-0 before the first play', () => {
    const state = stateAt(game, -1)
    expect(state.period).toBe(1)
    expect(state.clock).toBe('12:00')
    expect(state.home).toBe(0)
    expect(state.away).toBe(0)
    expect(state.homePercent).toBeUndefined()
    expect(state.changePercent).toBeUndefined()
    expect(state.scoring).toEqual([])
  })

  it('reports the opening price with no change at the start', () => {
    const state = stateAt(game, 0)
    expect(state.homePercent).toBe(61)
    expect(state.changePercent).toBe(0)
    expect(state.scoring).toEqual([])
  })

  it('lists points once when the scorer takes some back and then restores them', () => {
    const corrected: ReplayGame = {
      ...game,
      plays: [
        scoreAt(0, 52, 48),
        // Three for the home side while two are taken off the away side.
        scoreAt(1000, 55, 46),
        // The two are restored: not a basket.
        scoreAt(2000, 55, 48),
        // One play that carries a basket for each side.
        scoreAt(3000, 57, 50),
      ],
    }

    const { scoring } = stateAt(corrected, 3000)

    expect(scoring.map(each => [each.atMs, each.team, each.points, each.home, each.away])).toEqual([
      [3000, 'away', 2, 57, 50],
      [3000, 'home', 2, 57, 50],
      [1000, 'home', 3, 55, 46],
    ])
  })

  it('keeps at most six scoring plays, newest first', () => {
    const many: ReplayGame = {
      ...game,
      durationMs: 80_000,
      plays: Array.from({ length: 8 }, (_unused, i) => ({
        atMs: (i + 1) * 10_000,
        period: 1,
        clock: '0:00',
        home: i + 1,
        away: 0,
        team: 'home' as const,
        points: 1,
      })),
    }
    const state = stateAt(many, 80_000)
    expect(state.scoring).toHaveLength(6)
    expect(state.scoring[0]?.atMs).toBe(80_000)
  })
})

describe('priceLine', () => {
  it('plots each price up to the position and reaches to now', () => {
    const line = priceLine(game, { atMs: 12_000, ended: false, startedAtMs: 1_000 }, 1_000_000)
    expect(line).toEqual([
      { time: 1_000, value: 61 },
      { time: 11_000, value: 67 },
      { time: 1_000_000, value: 67 },
    ])
  })

  it('does not add a now point when the last price already reaches now', () => {
    const line = priceLine(game, { atMs: 12_000, ended: false, startedAtMs: 1_000 }, 11_000)
    expect(line).toEqual([
      { time: 1_000, value: 61 },
      { time: 11_000, value: 67 },
    ])
  })

  it('only includes prices at or before the position', () => {
    const line = priceLine(game, { atMs: 5_000, ended: false, startedAtMs: 1_000 }, 1_000)
    expect(line).toEqual([{ time: 1_000, value: 61 }])
  })
})

describe('replayDay', () => {
  it('formats an ISO date as a short month and day', () => {
    expect(replayDay('2026-11-19')).toBe('Nov 19')
    expect(replayDay('2026-01-03')).toBe('Jan 3')
  })
})

describe('schemas', () => {
  it('parses a whole game', () => {
    expect(() => ReplayGameSchema.parse(game)).not.toThrow()
  })

  it('rejects a negative atMs', () => {
    expect(ReplayPriceSchema.safeParse({ atMs: -1, bid: 0.5, ask: 0.6 }).success).toBe(false)
  })

  it('rejects a bid that is not below the ask', () => {
    expect(ReplayPriceSchema.safeParse({ atMs: 0, bid: 0.6, ask: 0.6 }).success).toBe(false)
  })

  it('rejects a team that is not home, away or null', () => {
    const base = { atMs: 0, period: 1, clock: '0:00', home: 0, away: 0, points: 0 }
    expect(ReplayPlaySchema.safeParse({ ...base, team: 'tie' }).success).toBe(false)
    expect(ReplayPlaySchema.safeParse({ ...base, team: null }).success).toBe(true)
    expect(ReplayPlaySchema.safeParse({ ...base, team: 'home' }).success).toBe(true)
  })
})
