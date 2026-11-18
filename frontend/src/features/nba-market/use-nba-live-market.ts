'use client'

import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'
import type { MarketScoreboardData } from '@/features/market-detail/types'
import { pinKey } from '@/features/signals/signal-catalogue'

import {
  NBA_PBP_SIGNAL,
  NBA_WINPROB_SIGNAL,
  marketStatusFromPlay,
  percentPoints,
  playIsThisGame,
  pregameScoreboard,
  priceSignalId,
  readPlay,
  roundTrace,
  scoreboardFromPlay,
} from './nba-live'

export interface NbaLiveState {
  /** The home side's market price over the window, in percent. */
  pricePoints: readonly ChartPoint[]
  /** The model's home win probability over the window, in percent. */
  modelPoints: readonly ChartPoint[]
  /** The scoreboard: the pregame placeholder until this round's game tips off. */
  scoreboard: MarketScoreboardData | undefined
  marketStatus: string
}

/**
 * The live game behind an NBA round, from the series' signals socket: the play-by-play
 * for the scoreboard and status, and the market's home price and the model's win
 * probability for the chart. `matchup` guards against the stream still showing the
 * previous game during the pregame; `range` is the round's window in unix milliseconds
 * and confines the traces to it.
 */
export function useNbaLiveMarket(
  apiUrl: string,
  series: string,
  matchup: { home: string; away: string } | undefined,
  range: { start: number; end: number } | undefined
): NbaLiveState {
  const priceId = priceSignalId(series)
  const { frames, history } = useSignalsStream(
    apiUrl,
    series,
    [NBA_PBP_SIGNAL, NBA_WINPROB_SIGNAL, priceId],
    range === undefined ? 0 : range.end - range.start
  )
  const latest = readPlay(frames[NBA_PBP_SIGNAL])
  const play = playIsThisGame(latest, matchup) ? latest : undefined
  const trace = (points: readonly ChartPoint[] | undefined) =>
    range === undefined ? percentPoints(points) : roundTrace(percentPoints(points), range.start)

  return {
    pricePoints: trace(history[pinKey(priceId, 'home')]),
    modelPoints: trace(history[pinKey(NBA_WINPROB_SIGNAL, 'home')]),
    scoreboard:
      play !== undefined
        ? scoreboardFromPlay(play)
        : matchup === undefined
          ? undefined
          : pregameScoreboard(),
    marketStatus: marketStatusFromPlay(play),
  }
}
