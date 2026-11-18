import type { CSSProperties } from 'react'

import type { MarketTeam } from '@/features/market-detail/types'

/**
 * The simulated league's six teams, mirrored from the exchange's league table
 * (`server/crates/exchange/src/sim/nba/league.rs`). The signals carry team codes; these are
 * the names and marks a person sees.
 */
export interface NbaTeam {
  code: string
  id: string
  city: string
  name: string
  logo: string
}

export const NBA_TEAMS: readonly NbaTeam[] = [
  { code: 'BOS', id: 'boston-celtics', city: 'Boston', name: 'Celtics', logo: 'boston-celtics' },
  {
    code: 'LAL',
    id: 'los-angeles-lakers',
    city: 'Los Angeles',
    name: 'Lakers',
    logo: 'los-angeles-lakers',
  },
  { code: 'NYK', id: 'new-york-knicks', city: 'New York', name: 'Knicks', logo: 'new-york-knicks' },
  {
    code: 'PHI',
    id: 'philadelphia-76ers',
    city: 'Philadelphia',
    name: '76ers',
    logo: 'philadelphia-76ers',
  },
  {
    code: 'SAS',
    id: 'san-antonio-spurs',
    city: 'San Antonio',
    name: 'Spurs',
    logo: 'san-antonio-spurs',
  },
  { code: 'TOR', id: 'toronto-raptors', city: 'Toronto', name: 'Raptors', logo: 'toronto-raptors' },
]

export function nbaTeam(code: string): NbaTeam | undefined {
  return NBA_TEAMS.find(team => team.code === code)
}

/**
 * Re-point the market's two side colours (chart lines, buy buttons, scoreboard labels,
 * card dots) at the teams' colours, which live as `--team-<code>` in the palette. A team
 * the palette does not know keeps the default side colour.
 */
export function sideColors(home: string, away: string): CSSProperties {
  const colors: Record<string, string> = {}
  if (nbaTeam(home) !== undefined) colors['--market-home'] = `var(--team-${home.toLowerCase()})`
  if (nbaTeam(away) !== undefined) colors['--market-away'] = `var(--team-${away.toLowerCase()})`
  // Custom properties are not in React's CSSProperties type; they are still valid inline style.
  return colors as CSSProperties
}

/** The team as the market header and scoreboard want it. */
export function marketTeam(code: string, side: MarketTeam['side']): MarketTeam {
  const team = nbaTeam(code)
  const name = team === undefined ? code : `${team.city} ${team.name}`
  return {
    abbreviation: code,
    id: team?.id ?? code.toLowerCase(),
    logo: { alt: `${name} logo`, src: `/assets/market/teams/${team?.logo ?? 'nba'}.svg` },
    ...(code === 'LAL' ? { logoInset: true } : {}),
    name,
    side,
  }
}
