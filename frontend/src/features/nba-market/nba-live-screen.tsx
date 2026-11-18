'use client'

import type { ComponentProps } from 'react'

import { MarketDetailScreen } from '@/features/market-detail/market-detail-screen'
import type { MarketDetail } from '@/features/market-detail/types'

import { sideColors } from './teams'
import { useNbaLiveMarket } from './use-nba-live-market'

type ScreenProps = Omit<ComponentProps<typeof MarketDetailScreen>, 'market'>

export interface NbaLiveScreenProps extends ScreenProps {
  market: MarketDetail
  config: { apiUrl: string; series: string }
}

/**
 * The market screen for an NBA round: the same chrome as every market, with the
 * scoreboard and the price chart driven by the live game on the series' signals socket.
 * The plays themselves stay off the page.
 */
export function NbaLiveScreen({ market, config, ...screen }: NbaLiveScreenProps) {
  const chart = market.probabilityChart
  const teams = market.header.teams
  const matchup =
    teams === undefined
      ? undefined
      : {
          home: teams.find(team => team.side === 'home')?.abbreviation ?? '',
          away: teams.find(team => team.side === 'away')?.abbreviation ?? '',
        }
  const live = useNbaLiveMarket(config.apiUrl, config.series, matchup, chart?.range)

  const liveMarket: MarketDetail = {
    ...market,
    ...(live.scoreboard === undefined ? {} : { scoreboard: live.scoreboard }),
    ...(chart === undefined
      ? {}
      : {
          probabilityChart: {
            ...chart,
            series: [
              { ...chart.series[0], points: live.pricePoints },
              { ...chart.series[1], points: [] },
            ],
          },
        }),
  }

  return (
    <div style={sideColors(matchup?.home ?? '', matchup?.away ?? '')}>
      <MarketDetailScreen {...screen} market={liveMarket} />
    </div>
  )
}
