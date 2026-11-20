import * as z from 'zod/mini'

/** The final score stands this long before the game starts again. */
export const HOLD_MS = 60_000
/** Each game in the index runs this far behind the one before it. */
export const PHASE_STEP_MS = 7 * 60_000

export interface ReplaySide {
  code: string
  name: string
}

export interface ReplayIndexEntry {
  id: string
  date: string
  home: ReplaySide
  away: ReplaySide
  durationMs: number
  final: { home: number; away: number }
}

export interface ReplayPlay {
  atMs: number
  period: number
  clock: string
  home: number
  away: number
  team: 'home' | 'away' | null
  points: number
}

export interface ReplayPrice {
  atMs: number
  bid: number
  ask: number
}

export interface ReplayGame extends ReplayIndexEntry {
  plays: ReplayPlay[]
  prices: ReplayPrice[]
}

const ReplaySideSchema = z.object({ code: z.string(), name: z.string() })

const indexFields = {
  id: z.string(),
  date: z.string(),
  home: ReplaySideSchema,
  away: ReplaySideSchema,
  durationMs: z.number(),
  final: z.object({ home: z.number(), away: z.number() }),
}

export const ReplayIndexEntrySchema = z.object(indexFields)

export const ReplayPlaySchema = z.object({
  atMs: z.number().check(z.gte(0)),
  period: z.number(),
  clock: z.string(),
  home: z.number(),
  away: z.number(),
  team: z.nullable(z.enum(['home', 'away'])),
  points: z.number(),
})

export const ReplayPriceSchema = z
  .object({ atMs: z.number().check(z.gte(0)), bid: z.number(), ask: z.number() })
  .check(z.refine(price => price.bid < price.ask, { message: 'bid must be below ask' }))

export const ReplayIndexSchema = z.array(ReplayIndexEntrySchema)

export const ReplayGameSchema = z.object({
  ...indexFields,
  plays: z.array(ReplayPlaySchema),
  prices: z.array(ReplayPriceSchema),
})

export interface ReplayPosition {
  atMs: number
  ended: boolean
  startedAtMs: number
}

export interface ReplayState {
  period: number
  clock: string
  home: number
  away: number
  homePercent: number | undefined
  changePercent: number | undefined
  scoring: ReplayPlay[]
}

const MONTHS = [
  'Jan',
  'Feb',
  'Mar',
  'Apr',
  'May',
  'Jun',
  'Jul',
  'Aug',
  'Sep',
  'Oct',
  'Nov',
  'Dec',
] as const

/** Shown scoring plays, newest first. */
const MAX_SCORING = 6

/** Rounds to one decimal place. */
function round1(value: number): number {
  return Math.round(value * 10) / 10
}

/** A price's midpoint as a percent (0–100), rounded to one decimal. */
function midpointPercent(price: ReplayPrice): number {
  return round1(((price.bid + price.ask) / 2) * 100)
}

/**
 * Where a game sits at `nowMs`. The game plays, holds on its final score for
 * `HOLD_MS`, then restarts; `index` offsets the phase so the games in the index
 * never all restart at once.
 */
export function positionAt(nowMs: number, durationMs: number, index: number): ReplayPosition {
  const cycle = durationMs + HOLD_MS
  const into = (((nowMs - index * PHASE_STEP_MS) % cycle) + cycle) % cycle
  const atMs = Math.min(into, durationMs)

  return { atMs, ended: into >= durationMs, startedAtMs: nowMs - into }
}

/** The score, clock and Kalshi price the game shows at `atMs`. */
const scoringByGame = new WeakMap<ReplayGame, readonly ReplayPlay[]>()

/**
 * The plays that put points on the board, oldest first, worked out once per game. The record
 * sometimes takes points back and later restores them; a side has scored only when its total
 * passes the highest it has been, so each point is listed once and a restoration is not a
 * basket. One play can carry a basket for each side.
 */
function scoringPlays(game: ReplayGame): readonly ReplayPlay[] {
  const known = scoringByGame.get(game)
  if (known !== undefined) return known
  const scoring: ReplayPlay[] = []
  let best = { away: game.plays[0]?.away ?? 0, home: game.plays[0]?.home ?? 0 }
  for (const play of game.plays.slice(1)) {
    for (const team of ['home', 'away'] as const) {
      const points = play[team] - best[team]
      if (points > 0) scoring.push({ ...play, points, team })
    }
    best = { away: Math.max(best.away, play.away), home: Math.max(best.home, play.home) }
  }
  scoringByGame.set(game, scoring)
  return scoring
}

export function stateAt(game: ReplayGame, atMs: number): ReplayState {
  let current: ReplayPlay | undefined
  for (const play of game.plays) {
    if (play.atMs <= atMs) {
      current = play
    } else {
      break
    }
  }

  let lastPrice: ReplayPrice | undefined
  for (const price of game.prices) {
    if (price.atMs <= atMs) {
      lastPrice = price
    } else {
      break
    }
  }
  const first = game.prices[0]
  const homePercent = lastPrice === undefined ? undefined : midpointPercent(lastPrice)
  const changePercent =
    homePercent === undefined || first === undefined
      ? undefined
      : round1(homePercent - midpointPercent(first))

  const scoring = scoringPlays(game)
    .filter(play => play.atMs <= atMs)
    .toReversed()
    .slice(0, MAX_SCORING)

  return {
    period: current?.period ?? 1,
    clock: current?.clock ?? '12:00',
    home: current?.home ?? 0,
    away: current?.away ?? 0,
    homePercent,
    changePercent,
    scoring,
  }
}

/**
 * The Kalshi price line up to `position`, placed on the real clock so it lines
 * up with the chart's time axis, and carried forward to `nowMs` so it reaches
 * the right edge.
 */
export function priceLine(
  game: ReplayGame,
  position: ReplayPosition,
  nowMs: number
): { time: number; value: number }[] {
  const points = game.prices
    .filter(price => price.atMs <= position.atMs)
    .map(price => ({ time: position.startedAtMs + price.atMs, value: midpointPercent(price) }))

  const last = points.at(-1)
  if (last !== undefined && last.time < nowMs) {
    points.push({ time: nowMs, value: last.value })
  }

  return points
}

/** Turns `'2026-11-19'` into `'Nov 19'`. */
export function replayDay(date: string): string {
  const [, month, day] = date.split('-')
  const monthName = MONTHS[Number(month) - 1] ?? month
  return `${monthName} ${Number(day)}`
}
