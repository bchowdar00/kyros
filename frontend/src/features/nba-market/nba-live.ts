import type { RustSignalFrame } from '@tiny/domain'
import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import type { MarketScoreboardData } from '@/features/market-detail/types'

/** The world's feeds, scoped to every NBA series. */
export const NBA_PBP_SIGNAL = 'nba.sim.pbp.v1'
export const NBA_WINPROB_SIGNAL = 'nba.sim.winprob.v1'

/** The series' own market price, keyed `home`/`away`. */
export function priceSignalId(series: string): string {
  return `${series}.current.price.v1`
}

/** One play, as the play-by-play frame carries it. */
export interface Play {
  seq: number
  action: string
  result: string
  team: string
  /** The player's name; empty on tipoff and the final. */
  player: string
  phase: 'in_play' | 'final'
  homeTeam: string
  awayTeam: string
  quarter: number
  clockRemaining: number
  homeScore: number
  awayScore: number
}

function num(values: RustSignalFrame['values'], key: string): number | undefined {
  const raw = values[key]
  if (raw === undefined || typeof raw === 'boolean') return undefined
  const value = Number(raw)
  return Number.isFinite(value) ? value : undefined
}

function text(values: RustSignalFrame['values'], key: string): string | undefined {
  const raw = values[key]
  return typeof raw === 'string' ? raw : undefined
}

/** Read a play from a frame; `undefined` for anything that is not a whole play. */
export function readPlay(frame: RustSignalFrame | undefined): Play | undefined {
  if (frame === undefined) return undefined
  const v = frame.values
  const seq = num(v, 'seq')
  const quarter = num(v, 'quarter')
  const clockRemaining = num(v, 'clock_remaining')
  const homeScore = num(v, 'home_score')
  const awayScore = num(v, 'away_score')
  const action = text(v, 'action')
  const phase = text(v, 'phase')
  const homeTeam = text(v, 'home_team')
  const awayTeam = text(v, 'away_team')
  if (
    seq === undefined ||
    quarter === undefined ||
    clockRemaining === undefined ||
    homeScore === undefined ||
    awayScore === undefined ||
    action === undefined ||
    homeTeam === undefined ||
    awayTeam === undefined ||
    (phase !== 'in_play' && phase !== 'final')
  )
    return undefined
  return {
    seq,
    action,
    result: text(v, 'result') ?? 'none',
    team: text(v, 'team') ?? homeTeam,
    player: text(v, 'player') ?? '',
    phase,
    homeTeam,
    awayTeam,
    quarter,
    clockRemaining,
    homeScore,
    awayScore,
  }
}

/** `Q1`–`Q4`, then `OT`, `2OT`, … */
export function periodLabel(quarter: number): string {
  if (quarter <= 4) return `Q${quarter}`
  const overtime = quarter - 4
  return overtime === 1 ? 'OT' : `${overtime}OT`
}

/** `m:ss` of the period clock. */
export function clockLabel(secondsRemaining: number): string {
  const total = Math.max(0, Math.round(secondsRemaining))
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  return `${minutes}:${String(seconds).padStart(2, '0')}`
}

export function scoreboardFromPlay(play: Play): MarketScoreboardData {
  return {
    awayScore: play.awayScore,
    clock: play.phase === 'final' ? '0:00' : clockLabel(play.clockRemaining),
    homeScore: play.homeScore,
    period: play.phase === 'final' ? 'Final' : periodLabel(play.quarter),
  }
}

/**
 * The scoreboard before this round's game has tipped off: no score and a full first-quarter
 * clock, so the header keeps its shape from the moment the round opens.
 */
export function pregameScoreboard(): MarketScoreboardData {
  return { awayScore: 0, clock: '12:00', homeScore: 0, period: 'Q1' }
}

/** The one-line game status a card shows: the period and clock, the final, or pregame. */
export function marketStatusFromPlay(play: Play | undefined): string {
  if (play === undefined) return 'Pregame'
  if (play.phase === 'final') return `Final · ${play.awayScore}–${play.homeScore}`
  return `${periodLabel(play.quarter)} · ${clockLabel(play.clockRemaining)}`
}

/** A probability trace (0..1 decimals) as the chart's percent points. */
export function percentPoints(points: readonly ChartPoint[] | undefined): readonly ChartPoint[] {
  return (points ?? []).map(point => ({
    time: point.time,
    value: Number((point.value * 100).toFixed(2)),
  }))
}

/**
 * A percent trace confined to the round: samples from before `startMs` belong to the
 * previous game (its final pins the price to a boundary) and are dropped, and the trace is
 * anchored at an even 50 on the open so the lines start together and part as the book forms.
 */
export function roundTrace(points: readonly ChartPoint[], startMs: number): readonly ChartPoint[] {
  const inRound = points.filter(point => point.time >= startMs)
  const first = inRound[0]
  return first !== undefined && first.time === startMs
    ? inRound
    : [{ time: startMs, value: 50 }, ...inRound]
}

/** Whether the play belongs to the matchup the round trades (the stream may still show the previous game). */
export function playIsThisGame(
  play: Play | undefined,
  matchup: { home: string; away: string } | undefined
): play is Play {
  return (
    play !== undefined &&
    matchup !== undefined &&
    play.homeTeam === matchup.home &&
    play.awayTeam === matchup.away
  )
}
