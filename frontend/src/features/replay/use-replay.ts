'use client'

import { useEffect, useState } from 'react'

import {
  positionAt,
  type ReplayGame,
  ReplayGameSchema,
  type ReplayIndexEntry,
  type ReplayPosition,
  type ReplayState,
  stateAt,
} from './replay'

export interface UseReplay {
  /** The game's file could not be read; there is nothing to wait for. */
  failed: boolean
  game: ReplayGame | undefined
  position: ReplayPosition
  state: ReplayState | undefined
  nowMs: number
}

/**
 * Plays a finished game back on a loop. Fetches the game once, then advances
 * the clock once a second so the score, price and scoring plays move. The game
 * stays undefined until it has loaded and parsed; a file that is missing or
 * does not read sets `failed`, so the page can say so and not wait for ever.
 */
export function useReplay(entry: ReplayIndexEntry, index: number): UseReplay {
  const [game, setGame] = useState<ReplayGame | undefined>(undefined)
  const [failed, setFailed] = useState(false)
  const [nowMs, setNowMs] = useState(() => Date.now())

  useEffect(() => {
    let active = true
    fetch(`/replays/${entry.id}.json`)
      .then(response => {
        if (!response.ok) throw new Error(`${response.status} for the replay file`)
        return response.json()
      })
      .then(body => ReplayGameSchema.parse(body))
      .then(parsed => {
        if (active) setGame(parsed)
      })
      .catch(() => {
        if (active) setFailed(true)
      })

    return () => {
      active = false
    }
  }, [entry.id])

  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 1000)

    return () => clearInterval(id)
  }, [])

  const position = positionAt(nowMs, entry.durationMs, index)
  const state = game === undefined ? undefined : stateAt(game, position.atMs)

  return { failed, game, position, state, nowMs }
}
