'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { exchangeMarketKey } from '@kyros/domain'
import { formatExchangeDollars } from '@kyros/money'

import { useIdentityStore } from '@/features/identity/identity-store'

import { useGeminiSeries } from '../gemini/use-gemini-series.query'
import { MarketDetailScreen } from '../market-detail-screen'
import type { MarketDetail, MyStrategy } from '../types'
import { STRATEGY_RANK_INTERVAL_MS, accountRows, rankedRows, strategyRank } from './accounts.mapper'
import { exchangeBook, exchangeOutcomes } from './book.mapper'
import {
  availableExchangeCents,
  quoteExchangeBuy,
  tradingUnavailable,
  type MarketTrading,
} from './buy'
import { deployUnavailable, type StrategyDeploy } from './deploy'
import { ExchangeOpenOrders } from './open-orders'
import { ExchangeTradeHistory } from './trade-history'
import { createRustExchangeStore } from './rust-exchange-store'

export interface RustExchangeConfigProps {
  apiUrl: string
  series: string
}

interface ScreenProps {
  market: MarketDetail
  config: RustExchangeConfigProps
}

/** The app-level identity gate guarantees an address; this is only the fail-closed fallback. */
export function RustExchangeMarketScreen({ market, config }: ScreenProps) {
  const identity = useIdentityStore(state => state.identity)
  const [queryClient] = useState(() => new QueryClient())

  if (identity === null) {
    return (
      <MarketDetailScreen
        market={market}
        signalsApi={config}
        exchangeStatus={<p className="py-lg text-label text-muted-foreground">Sign in to trade.</p>}
      />
    )
  }

  return (
    <QueryClientProvider client={queryClient}>
      <ConnectedMarketScreen
        address={identity.address}
        config={config}
        key={`${identity.address}:${config.series}`}
        market={market}
      />
    </QueryClientProvider>
  )
}

/**
 * The strategy order by size, re-ranked on a timer rather than on every frame so the
 * rows hold still while positions tick. The latest rows are read through a ref so the
 * interval survives re-renders.
 */
function useStrategyRank(rows: readonly MyStrategy[]): readonly string[] {
  const [rank, setRank] = useState<readonly string[]>([])
  const latest = useRef(rows)
  useEffect(() => {
    latest.current = rows
  })
  useEffect(() => {
    const timer = setInterval(
      () => setRank(strategyRank(latest.current)),
      STRATEGY_RANK_INTERVAL_MS
    )
    return () => clearInterval(timer)
  }, [])
  return rank
}

function ConnectedMarketScreen({ address, market, config }: ScreenProps & { address: string }) {
  const setBalance = useIdentityStore(state => state.setBalance)
  const [store] = useState(() =>
    createRustExchangeStore({
      apiUrl: config.apiUrl,
      address,
      series: config.series,
      onBalance: setBalance,
    })
  )
  const { feed, account, notice, pending, deploying, deployNotice } = useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getSnapshot
  )
  const geminiBook = useGeminiSeries(config.apiUrl, config.series, market.gemini)
  const trading: MarketTrading = {
    availableBalanceCents:
      account.status === 'live' ? availableExchangeCents(account.available) : undefined,
    reason: pending ? 'Waiting for the exchange.' : tradingUnavailable(feed, account, geminiBook),
    notice,
    quote: (amount, side) => quoteExchangeBuy(feed, account, amount, side),
    submit: quote => void store.buy(quote, geminiBook),
  }
  const deploy: StrategyDeploy = {
    availableBalanceCents:
      account.status === 'live' ? availableExchangeCents(account.available) : undefined,
    reason: deployUnavailable(account, deploying),
    notice: deployNotice,
    submit: request => store.deployTemplate(request),
  }
  const position = account.positions.find(
    item => feed.marketId !== null && item.market_id === exchangeMarketKey(feed.marketId)
  )
  const rows = accountRows(account.accounts, config.series, market.header.outcomes)
  const rank = useStrategyRank(rows)
  const liveMarket = {
    ...market,
    header: { ...market.header, outcomes: exchangeOutcomes(market.header.outcomes, feed) },
    // The live book, under the series' own column labels.
    orderBook: {
      ...exchangeBook(feed),
      ...(market.orderBook === undefined ? {} : { columnLabels: market.orderBook.columnLabels }),
      geminiBook,
    },
    ...(market.myStrategies === undefined
      ? {}
      : { myStrategies: { ...market.myStrategies, rows: rankedRows(rows, rank) } }),
  }

  const exchangeNotice =
    geminiBook.gemini?.orders === 'real'
      ? `Manual orders here are pretend money against the live order book. Strategies run by this server's operator ${
          geminiBook.gemini.env === 'sandbox'
            ? "send orders to Gemini's sandbox."
            : 'send real orders to Gemini.'
        }`
      : 'Paper mode. Pretend money against the live order book. No order leaves this site.'

  const screen = {
    trading,
    deploy,
    exchangeMarketId: feed.marketId,
    signalsApi: config,
    exchangeOrders: (
      <ExchangeOpenOrders account={account} outcomes={market.header.outcomes} store={store} />
    ),
    exchangeHistory: <ExchangeTradeHistory account={account} outcomes={market.header.outcomes} />,
  }

  return (
    <MarketDetailScreen
      {...screen}
      market={liveMarket}
      exchangeStatus={
        <section
          className="flex flex-col gap-xs border-t-2 border-foreground pt-md text-label text-muted-foreground"
          aria-label="Exchange connection"
        >
          <strong className="text-foreground">{exchangeNotice}</strong>
          <output>
            Book: {feed.status} · Account: {account.status} · Round {feed.marketId ?? '—'}:{' '}
            {feed.state ?? 'waiting'}
          </output>
          <div className="flex flex-wrap gap-lg">
            <span>
              Balance: {account.available === null ? '—' : formatExchangeDollars(account.available)}
            </span>
            <span>
              Locked: {account.locked === null ? '—' : formatExchangeDollars(account.locked)}
            </span>
            <span>
              Position:{' '}
              {account.status !== 'live' || feed.marketId === null
                ? '—'
                : `${position?.yes ?? '0'} ${market.header.outcomes[0].label} / ${position?.no ?? '0'} ${market.header.outcomes[1].label}`}
            </span>
          </div>
          {trading.reason && <p>{trading.reason}</p>}
        </section>
      }
    />
  )
}
