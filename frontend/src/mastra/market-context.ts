import { createVenueRegistry } from './capabilities/strategy/registry'
import { sidesOf } from './capabilities/strategy/sides'
import { STANDALONE_CHAT } from './request-context'

export { STANDALONE_CHAT }

function roundLength(durationSecs: number) {
  if (durationSecs % 3600 === 0) return `${durationSecs / 3600}-hour`
  if (durationSecs % 60 === 0) return `${durationSecs / 60}-minute`
  return `${durationSecs}-second`
}

/**
 * The one thing the page already decided that Teeny must never ask: which market the
 * conversation is about. Sent as a per-request system message so the same agent serves
 * every market page and the standalone chat.
 */
export async function marketSystemMessage(apiUrl: string | undefined, marketSlug: string) {
  if (marketSlug === STANDALONE_CHAT)
    return 'This chat was opened outside a market page, so no market is selected. Strategies cannot be built here; say so if asked.'
  const series = apiUrl ? await createVenueRegistry(apiUrl).series(marketSlug) : undefined
  const name = series
    ? series.underlying === 'nba'
      ? `the ${roundLength(series.duration_secs)} NBA head-to-head market (one simulated game per round, home against away)`
      : `the ${roundLength(series.duration_secs)} BTC up/down market`
    : 'this market'
  const { quoted, other } = sidesOf(series?.outcomes)
  const sides = series
    ? `Its two sides are ${quoted} and ${other}; the book quotes ${quoted}, and ${other} is one dollar minus ${quoted}. `
    : ''
  const matchup =
    series?.underlying === 'nba' &&
    series.reference !== undefined &&
    'home_team' in series.reference
      ? `The game this round trades is ${series.reference.away_team} at ${series.reference.home_team}, so "${series.reference.home_team}" means home and "${series.reference.away_team}" means away. `
      : ''
  return `This chat is open on ${name} (${marketSlug}). ${sides}${matchup}Every strategy built in this chat trades this market and no other. Never ask which market the user means; call it "${name}" when it matters.`
}
