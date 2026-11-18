import { describe, expect, it } from 'vitest'

import { sideColors } from './teams'

import {
  clockLabel,
  marketStatusFromPlay,
  percentPoints,
  periodLabel,
  playIsThisGame,
  pregameScoreboard,
  priceSignalId,
  readPlay,
  roundTrace,
  scoreboardFromPlay,
} from './nba-live'

const frame = (values: Record<string, string | number>) => ({
  signal_id: 'nba.sim.pbp.v1',
  ts_ms: 1,
  quality: { state: 'ready', reasons: [] },
  values,
})

const three = frame({
  seq: 41,
  action: 'three_pointer',
  result: 'made',
  team: 'BOS',
  player: 'Jayson Tatum',
  phase: 'in_play',
  home_team: 'BOS',
  away_team: 'SAS',
  quarter: 2,
  home_score_before: 41,
  away_score_before: 38,
  home_score: 44,
  away_score: 38,
  clock_remaining: '302.4',
})

describe('nba live model', () => {
  it('reads a play from a frame and rejects a partial one', () => {
    const play = readPlay(three)
    expect(play?.seq).toBe(41)
    expect(play?.player).toBe('Jayson Tatum')
    expect(play?.clockRemaining).toBe(302.4)
    expect(play?.phase).toBe('in_play')
    expect(readPlay(frame({ seq: 1 }))).toBeUndefined()
    expect(readPlay(undefined)).toBeUndefined()
  })

  it('formats the scoreboard and the status', () => {
    const play = readPlay(three)
    if (play === undefined) throw new Error('play')
    expect(scoreboardFromPlay(play)).toEqual({
      awayScore: 38,
      clock: '5:02',
      homeScore: 44,
      period: 'Q2',
    })
    expect(marketStatusFromPlay(play)).toBe('Q2 · 5:02')
    expect(marketStatusFromPlay(undefined)).toBe('Pregame')
    expect(pregameScoreboard()).toEqual({
      awayScore: 0,
      clock: '12:00',
      homeScore: 0,
      period: 'Q1',
    })
    const final = readPlay(
      frame({
        ...three.values,
        seq: 600,
        action: 'final',
        phase: 'final',
        player: '',
        clock_remaining: '0.0',
        quarter: 4,
      })
    )
    if (final === undefined) throw new Error('final')
    expect(scoreboardFromPlay(final).period).toBe('Final')
    expect(marketStatusFromPlay(final)).toBe('Final · 38–44')
    expect(periodLabel(5)).toBe('OT')
    expect(periodLabel(6)).toBe('2OT')
    expect(clockLabel(59.6)).toBe('1:00')
  })

  it('guards on the matchup', () => {
    const play = readPlay(three)
    if (play === undefined) throw new Error('play')
    expect(playIsThisGame(play, { home: 'BOS', away: 'SAS' })).toBe(true)
    expect(playIsThisGame(play, { home: 'LAL', away: 'SAS' })).toBe(false)
    expect(playIsThisGame(play, undefined)).toBe(false)
  })

  it('re-points the side colours at the teams', () => {
    expect(sideColors('BOS', 'SAS')).toEqual({
      '--market-home': 'var(--team-bos)',
      '--market-away': 'var(--team-sas)',
    })
    expect(sideColors('BOS', 'XYZ')).toEqual({ '--market-home': 'var(--team-bos)' })
  })

  it('maps probability traces to percent and names the price signal', () => {
    expect(percentPoints([{ time: 1, value: 0.645 }])).toEqual([{ time: 1, value: 64.5 }])
    expect(percentPoints(undefined)).toEqual([])
    expect(priceSignalId('nba-h2h')).toBe('nba-h2h.current.price.v1')
  })

  it('confines a trace to the round and anchors it at even', () => {
    const points = [
      { time: 900, value: 99.9 },
      { time: 1_200, value: 61 },
      { time: 1_500, value: 58 },
    ]
    expect(roundTrace(points, 1_000)).toEqual([
      { time: 1_000, value: 50 },
      { time: 1_200, value: 61 },
      { time: 1_500, value: 58 },
    ])
    expect(roundTrace([], 1_000)).toEqual([{ time: 1_000, value: 50 }])
    expect(roundTrace([{ time: 1_000, value: 62 }], 1_000)).toEqual([{ time: 1_000, value: 62 }])
  })
})
