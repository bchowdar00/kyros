import { readFileSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'

import { ReplayGameSchema, ReplayIndexSchema } from './replay'

// Vitest runs from the frontend workspace root.
const replaysDir = join(process.cwd(), 'public', 'replays')
const indexPath = join(process.cwd(), 'src', 'features', 'replay', 'replay-index.json')

const index = ReplayIndexSchema.parse(JSON.parse(readFileSync(indexPath, 'utf-8')))
const files = readdirSync(replaysDir).filter(name => name.endsWith('.json'))
const games = files.map(name =>
  ReplayGameSchema.parse(JSON.parse(readFileSync(join(replaysDir, name), 'utf-8')))
)

/**
 * Games whose raw ESPN play-by-play lowers a running score by a point or two
 * (a scoring correction mid-game), reported by the generator. For these the
 * "never decreases" check is relaxed to "the final equals the last play";
 * the data is not edited by hand.
 */
/** The most a correction takes off one side in one play: a three-pointer. */
const LARGEST_CORRECTION = 3
const SCORE_CORRECTIONS = new Set(['401910002', '401910003', '401910007', '401910008', '401910013'])

describe('generated replay data', () => {
  it('has at least three games', () => {
    expect(games.length).toBeGreaterThanOrEqual(3)
  })

  it('indexes exactly the games on disk', () => {
    const onDisk = files.map(name => name.replace(/\.json$/, '')).toSorted()
    const indexed = index.map(entry => entry.id).toSorted()
    expect(indexed).toEqual(onDisk)
  })

  it('orders the index by final margin, then id', () => {
    const keys = index.map(entry => ({
      margin: Math.abs(entry.final.home - entry.final.away),
      id: entry.id,
    }))
    const sorted = keys.toSorted((a, b) => a.margin - b.margin || a.id.localeCompare(b.id))
    expect(keys).toEqual(sorted)
  })

  it.each(files)('%s has a sound, shortened timeline', name => {
    const game = games[files.indexOf(name)]
    if (game === undefined) throw new Error('missing game')

    expect(game.plays.length).toBeGreaterThan(0)
    for (let i = 1; i < game.plays.length; i += 1) {
      const previous = game.plays[i - 1]
      const play = game.plays[i]
      if (previous === undefined || play === undefined) throw new Error('missing play')
      expect(play.atMs).toBeGreaterThanOrEqual(previous.atMs)
      expect(play.atMs - previous.atMs).toBeLessThanOrEqual(8000)
    }

    const last = game.plays.at(-1)
    if (last === undefined) throw new Error('missing last play')
    expect(game.durationMs).toBe(last.atMs)
    expect(game.final).toEqual({ home: last.home, away: last.away })

    // A correction takes back a basket, never more; any other game never loses a point.
    const mayDrop = SCORE_CORRECTIONS.has(game.id) ? LARGEST_CORRECTION : 0
    for (let i = 1; i < game.plays.length; i += 1) {
      const previous = game.plays[i - 1]
      const play = game.plays[i]
      if (previous === undefined || play === undefined) throw new Error('missing play')
      expect(previous.home - play.home).toBeLessThanOrEqual(mayDrop)
      expect(previous.away - play.away).toBeLessThanOrEqual(mayDrop)
      expect(play.period).toBeGreaterThanOrEqual(previous.period)
    }
  })

  it.each(files)('%s says the same as its line in the index', name => {
    const game = games[files.indexOf(name)]
    const entry = index.find(each => each.id === game?.id)
    if (game === undefined || entry === undefined) throw new Error('missing game')

    // The page asks for a game by its id, so a file must hold the game it is named for.
    expect(game.id).toBe(name.replace(/\.json$/, ''))
    expect(entry).toEqual({
      away: game.away,
      date: game.date,
      durationMs: game.durationMs,
      final: game.final,
      home: game.home,
      id: game.id,
    })
  })

  it.each(files)('%s has prices inside the window', name => {
    const game = games[files.indexOf(name)]
    if (game === undefined) throw new Error('missing game')

    expect(game.prices.length).toBeGreaterThanOrEqual(30)
    // The playback stops reading at the first price past its position, so they must be in order.
    expect(game.prices.map(price => price.atMs)).toEqual(
      game.prices.map(price => price.atMs).toSorted((a, b) => a - b)
    )
    for (const price of game.prices) {
      expect(price.bid).toBeGreaterThan(0)
      expect(price.bid).toBeLessThan(price.ask)
      expect(price.ask).toBeLessThan(1)
      expect(price.atMs).toBeGreaterThanOrEqual(0)
      expect(price.atMs).toBeLessThanOrEqual(game.durationMs)
    }
  })
})
