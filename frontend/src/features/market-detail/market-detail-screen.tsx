import type { ReactNode } from 'react'
import type { MarketTrading } from './exchange/buy'
import type { StrategyDeploy } from './exchange/deploy'
import { LiveMarketPoller } from './live-market-poller'
import { MarketDetailLayout } from './market-detail-layout'
import { headerCountdownRange } from './active-market.mapper'
import { MarketHeader } from './market-header'
import { MarketPriceCard } from './market-price-card'
import { MarketProbabilityCard } from './market-probability-card'
import { MyStrategiesPanel } from './my-strategies-panel'
import { NbaScoreboard } from './nba-scoreboard'
import { OrderBookPanel } from './order-book-panel'
import { LiveSignalsSection } from './live-signals-section'
import { SignalsSection } from './signals-section'
import { ChatPanel } from './chat-panel'
import type { MarketDetail } from './types'

interface MarketDetailScreenProps {
  market: MarketDetail
  trading?: MarketTrading | undefined
  /** How the Run / Inverse widget deploys a template; absent without a live exchange. */
  deploy?: StrategyDeploy | undefined
  exchangeStatus?: ReactNode
  exchangeOrders?: ReactNode
  exchangeHistory?: ReactNode
  exchangeMarketId?: string | null
  /** Where the browser reads the live signal catalogue; absent for static fixtures. */
  signalsApi?: { apiUrl: string; series: string } | undefined
}

export function MarketDetailScreen({
  market,
  trading,
  deploy,
  exchangeStatus,
  exchangeOrders,
  exchangeHistory,
  exchangeMarketId,
  signalsApi,
}: MarketDetailScreenProps) {
  const marketKey = `${market.slug}:${market.liveEvent?.eventId ?? market.slug}:${exchangeMarketId ?? 'catalog'}`
  const commentary = market.priceChart?.commentary ?? market.probabilityChart?.commentary

  return (
    <>
      {market.liveEvent ? (
        <LiveMarketPoller {...market.liveEvent} key={market.liveEvent.seriesId} />
      ) : null}
      <MarketDetailLayout
        marketColumn={
          <>
            <MarketHeader
              {...market.header}
              tradingUnavailableReason={
                trading === undefined ? 'Exchange unavailable' : trading.reason
              }
              marketKey={marketKey}
              marketSlug={market.slug}
              range={headerCountdownRange(market)}
            />

            {market.scoreboard ? (
              <NbaScoreboard
                scoreboard={market.scoreboard}
                signalsApi={signalsApi}
                key={`scoreboard:${marketKey}`}
              />
            ) : null}

            {/* The series keeps the chart scale and animation alive across event rollover. */}
            <div
              className="flex flex-col gap-5xl pb-5xl"
              key={market.liveEvent?.seriesId ?? market.slug}
            >
              <div className="flex flex-col gap-lg pt-xl">
                {market.priceChart ? (
                  <MarketPriceCard {...market.priceChart} />
                ) : market.probabilityChart ? (
                  <MarketProbabilityCard {...market.probabilityChart} />
                ) : null}
              </div>

              {commentary ? (
                <p className="text-body font-regular text-foreground">{commentary}</p>
              ) : null}

              {market.signals && signalsApi ? (
                <LiveSignalsSection
                  apiUrl={signalsApi.apiUrl}
                  fallback={market.signals}
                  key={`signals:${signalsApi.series}`}
                  series={signalsApi.series}
                />
              ) : market.signals ? (
                <SignalsSection {...market.signals} key={`signals:${marketKey}`} />
              ) : null}

              {market.myStrategies ? (
                <MyStrategiesPanel
                  {...market.myStrategies}
                  apiUrl={signalsApi?.apiUrl}
                  key={`strategies:${marketKey}`}
                  openOrders={exchangeOrders}
                  tradeHistory={exchangeHistory}
                />
              ) : null}

              {market.orderBook ? (
                <OrderBookPanel {...market.orderBook} key={`book:${marketKey}`} />
              ) : null}
              {exchangeStatus}
            </div>
          </>
        }
        marketSlug={market.slug}
        strategyColumn={
          market.chatPanel ? (
            <ChatPanel
              trading={trading}
              deploy={deploy}
              exchangeApi={signalsApi}
              // The LLM author writes Bitcoin strategies only; an NBA game turns it off.
              authoringUnavailable={market.scoreboard !== undefined}
              scopeLabel={market.header.title}
              data={market.chatPanel}
              marketSlug={market.slug}
              marketKey={marketKey}
              outcomes={market.header.outcomes}
              exchange={signalsApi}
              key={market.slug}
            />
          ) : undefined
        }
      />
    </>
  )
}
