const utcTime = new Intl.DateTimeFormat('en-US', {
  day: 'numeric',
  hour: '2-digit',
  hourCycle: 'h23',
  minute: '2-digit',
  month: 'short',
  timeZone: 'UTC',
})

const seriesLabels: Record<string, string> = {
  'btc-15m': 'BTC 15m',
  'btc-5m': 'BTC 5m',
  'nba-h2h': 'NBA game',
}

/** A market series as people call it; one this file does not know is shown as given. */
export function seriesLabel(series: string): string {
  return seriesLabels[series] ?? series
}

/** `0x1234…abcd`: enough of an id or address to match it on the explorer. */
export function shortHex(hex: string): string {
  return `${hex.slice(0, 6)}…${hex.slice(-4)}`
}

/** A block time as `Nov 20, 08:25 UTC`. The explorer's stamps are UTC and are shown as UTC. */
export function atUtc(iso: string): string {
  const at = new Date(iso)
  // Formatting an unreadable time throws; one odd stamp must not take the page down.
  return Number.isNaN(at.getTime()) ? 'Time not given' : `${utcTime.format(at)} UTC`
}

/** A path under an origin, whether or not the origin ends in a slash. */
export function under(base: string, path: string): string {
  return new URL(path, base.endsWith('/') ? base : `${base}/`).href
}

/** The strategy's name, or its id once the exchange no longer holds it. */
export function strategyName(strategy: { deploymentId: string; name: string | undefined }): string {
  return strategy.name ?? `Strategy ${shortHex(strategy.deploymentId)}`
}

/** Rounds won out of rounds sealed, as a whole percentage. */
export function winRate(record: { rounds: number; wonRounds: number }): string {
  return `${record.rounds === 0 ? 0 : Math.round((record.wonRounds / record.rounds) * 100)}%`
}
