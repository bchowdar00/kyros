'use client'

import type { RustSignalFrame } from '@kyros/domain'
import { useBrowserTimeZone } from '@kyros/ui/lib/use-browser-time-zone'

import type { SignalsState } from '@/features/exchange-streams/signals-stream'
import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'

/** The play-by-play signal, scoped to the underlying (it keeps the last game's final). */
const PBP_SIGNAL_ID = 'nba.live.pbp.v1'
const REGULATION_QUARTERS = 4
const SECONDS_PER_MINUTE = 60

type SignalValues = RustSignalFrame['values']

interface ScoreboardTeams {
  away: string
  home: string
  tipoffMs?: number
}

function readString(values: SignalValues, key: string): string | undefined {
  const value = values[key]
  return typeof value === 'string' ? value : undefined
}

function readNumber(values: SignalValues, key: string): number | undefined {
  const value = values[key]
  if (typeof value === 'number') return value
  if (typeof value === 'string' && value.trim() !== '' && Number.isFinite(Number(value))) {
    return Number(value)
  }
  return undefined
}

/** `Q1`–`Q4`, then `OT`, `OT2`, … for anything past regulation. */
function periodLabel(quarter: number): string {
  if (quarter <= REGULATION_QUARTERS) return `Q${quarter}`
  const overtime = quarter - REGULATION_QUARTERS
  return overtime === 1 ? 'OT' : `OT${overtime}`
}

function clockTime(secondsLeft: number): string {
  const whole = Math.max(0, Math.floor(secondsLeft))
  const minutes = Math.floor(whole / SECONDS_PER_MINUTE)
  const seconds = whole % SECONDS_PER_MINUTE
  return `${minutes}:${String(seconds).padStart(2, '0')}`
}

function tipoffTime(tipoffMs: number, timeZone: string): string {
  return new Intl.DateTimeFormat('en-US', { hour: 'numeric', minute: '2-digit', timeZone }).format(
    tipoffMs
  )
}

/** The game clock line, from the matching frame's phase or, before tipoff, the scheduled time. */
function clockLine(
  matched: boolean,
  values: SignalValues | undefined,
  teams: ScoreboardTeams,
  timeZone: string
): string | undefined {
  const phase = matched && values !== undefined ? readString(values, 'phase') : undefined

  if (phase === 'in_play' && values !== undefined) {
    const quarter = readNumber(values, 'quarter')
    const remaining = readNumber(values, 'clock_remaining')
    if (quarter !== undefined && remaining !== undefined) {
      return `${periodLabel(quarter)} ${clockTime(remaining)}`
    }
  }
  if (phase === 'final') return 'Final'

  // With no frame for this game the scheduled tipoff is all that is known. It is stated as a
  // time, not as "in the future", so the line stays true without reading the clock.
  if (teams.tipoffMs !== undefined && (phase === 'pregame' || !matched)) {
    return `Tipoff ${tipoffTime(teams.tipoffMs, timeZone)}`
  }
  return undefined
}

/** The last play, built from what the frame actually says; nothing on `other` or no match. */
function lastPlayLine(matched: boolean, values: SignalValues | undefined): string | undefined {
  if (!matched || values === undefined) return undefined
  const action = readString(values, 'action')
  const team = readString(values, 'team')
  if (action === undefined || action === 'other' || team === undefined) return undefined

  let line = `${team} ${action.replaceAll('_', '-')}`
  const result = readString(values, 'result')
  if (result !== undefined && result !== 'none') line += ` ${result}`
  const player = readString(values, 'player')
  if (player !== undefined && player !== '') line += `, ${player}`
  return line
}

/** `away` first, as the matchup reads; a score the frame did not carry shows as an em dash. */
function scoreText(team: string, matched: boolean, score: number | undefined): string {
  return `${team} ${matched && score !== undefined ? score : '—'}`
}

/**
 * The score strip. Given the latest `nba.live.pbp.v1` values (or none), it shows the two
 * teams, their score when the frame is this game's, the clock, and the last play. It never
 * prints a zero it did not read: a missing or mismatched frame shows em dashes. When the feed
 * is disconnected the last score stays up and the strip says it may be behind.
 */
export function NbaScoreboardStrip({
  feed,
  scoreboard,
  values,
}: {
  feed?: SignalsState['status'] | undefined
  scoreboard: ScoreboardTeams
  values?: SignalValues | undefined
}) {
  const timeZone = useBrowserTimeZone()
  const matched =
    values !== undefined &&
    readString(values, 'home_team') === scoreboard.home &&
    readString(values, 'away_team') === scoreboard.away

  const awayScore = scoreText(scoreboard.away, matched, readNumber(values ?? {}, 'away_score'))
  const homeScore = scoreText(scoreboard.home, matched, readNumber(values ?? {}, 'home_score'))
  const clock = clockLine(matched, values, scoreboard, timeZone)
  const lastPlay = lastPlayLine(matched, values)

  return (
    <section
      aria-label="Game score"
      className="flex flex-wrap items-baseline gap-x-lg gap-y-xs py-md"
    >
      <p aria-live="polite" className="flex items-baseline gap-sm font-display text-heading-sm">
        <span className="text-foreground">{awayScore}</span>
        <span aria-hidden="true" className="text-muted-foreground">
          ·
        </span>
        <span className="text-foreground">{homeScore}</span>
      </p>
      {clock === undefined ? null : (
        <p className="text-label font-medium text-muted-foreground">{clock}</p>
      )}
      {lastPlay === undefined ? null : (
        <p className="text-label font-regular text-muted-foreground">{lastPlay}</p>
      )}
      {feed === 'disconnected' ? (
        <p className="text-label font-medium text-muted-foreground">Score feed disconnected</p>
      ) : null}
    </section>
  )
}

function LiveNbaScoreboard({
  apiUrl,
  scoreboard,
  series,
}: {
  apiUrl: string
  scoreboard: ScoreboardTeams
  series: string
}) {
  const { frames, status } = useSignalsStream(apiUrl, series, [PBP_SIGNAL_ID])
  return (
    <NbaScoreboardStrip
      feed={status}
      scoreboard={scoreboard}
      values={frames[PBP_SIGNAL_ID]?.values}
    />
  )
}

/**
 * The live score strip for an NBA round. It reads the play-by-play off the series' shared
 * `/ws/signals` socket (the same one the signals section uses), so it opens no second
 * connection; without a live exchange it shows the teams and the scheduled tipoff.
 */
export function NbaScoreboard({
  scoreboard,
  signalsApi,
}: {
  scoreboard: ScoreboardTeams
  signalsApi?: { apiUrl: string; series: string } | undefined
}) {
  return signalsApi === undefined ? (
    <NbaScoreboardStrip scoreboard={scoreboard} />
  ) : (
    <LiveNbaScoreboard
      apiUrl={signalsApi.apiUrl}
      scoreboard={scoreboard}
      series={signalsApi.series}
    />
  )
}
