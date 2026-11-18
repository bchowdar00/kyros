import type { MarketScoreboardData, MarketTeam } from './types'

export interface MarketScoreboardProps {
  scoreboard: MarketScoreboardData
  teams: readonly [MarketTeam, MarketTeam]
}

export function MarketScoreboard({ scoreboard, teams }: MarketScoreboardProps) {
  const [awayTeam, homeTeam] = teams

  return (
    <section aria-label="Market scoreboard" className="flex h-[46px] items-center pb-[2px]">
      <div className="flex min-w-0 flex-1 flex-col gap-sm text-center">
        <p className="text-label font-medium tracking-label text-market-away">
          {awayTeam.abbreviation}
        </p>
        <p className="font-mono text-2xl font-regular text-foreground">{scoreboard.awayScore}</p>
      </div>

      <div aria-hidden="true" className="h-3xl w-px shrink-0 bg-border-subtle" />

      <div className="flex min-w-0 flex-1 flex-col gap-sm text-center">
        <p className="text-label font-medium tracking-label text-muted-foreground">
          {scoreboard.period}
        </p>
        <p className="font-mono text-2xl font-regular text-foreground">{scoreboard.clock}</p>
      </div>

      <div aria-hidden="true" className="h-3xl w-px shrink-0 bg-border-subtle" />

      <div className="flex min-w-0 flex-1 flex-col gap-sm text-center">
        <p className="text-label font-medium tracking-label text-market-home">
          {homeTeam.abbreviation}
        </p>
        <p className="font-mono text-2xl font-regular text-foreground">{scoreboard.homeScore}</p>
      </div>
    </section>
  )
}
