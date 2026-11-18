'use client'

import { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { exchangeMarketKey } from '@tiny/domain'
import { formatExchangeBalance } from '@tiny/money'

import { useIdentityStore } from '@/features/identity/identity-store'
import { NbaLiveScreen } from '@/features/nba-market/nba-live-screen'

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
    <ConnectedMarketScreen
      address={identity.address}
      config={config}
      key={`${identity.address}:${config.series}`}
      market={market}
    />
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
  const trading: MarketTrading = {
    availableBalanceCents:
      account.status === 'live' ? availableExchangeCents(account.available) : undefined,
    reason: pending ? 'Waiting for the exchange.' : tradingUnavailable(feed, account),
    notice,
    quote: (amount, side) => quoteExchangeBuy(feed, account, amount, side),
    submit: quote => void store.buy(quote),
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
    // The live book, under the series' own column labels (HOME for an NBA round).
    orderBook: {
      ...exchangeBook(feed),
      ...(market.orderBook === undefined ? {} : { columnLabels: market.orderBook.columnLabels }),
    },
    ...(market.myStrategies === undefined
      ? {}
      : { myStrategies: { ...market.myStrategies, rows: rankedRows(rows, rank) } }),
  }

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

  if (market.probabilityChart !== undefined) {
    return (
      <NbaLiveScreen
        {...screen}
        config={config}
        market={liveMarket}
        exchangeStatus={
          <ExchangeConnection config={config} feed={feed} account={account} trading={trading} />
        }
      />
    )
  }

  return (
    <MarketDetailScreen
      {...screen}
      market={liveMarket}
      exchangeStatus={
        <section
          className="flex flex-col gap-sm rounded-md border border-border-subtle bg-surface-raised p-lg text-label"
          aria-label="Exchange connection"
        >
          <strong>Local exchange · {config.series}</strong>
          <output>
            Book: {feed.status} · Account: {account.status} · Round {feed.marketId ?? '—'}:{' '}
            {feed.state ?? 'waiting'}
          </output>
          <div className="flex flex-wrap gap-lg">
            <span>
              Balance: {account.available === null ? '—' : formatExchangeBalance(account.available)}
            </span>
            <span>
              Locked: {account.locked === null ? '—' : formatExchangeBalance(account.locked)}
            </span>
            <span>
              Position:{' '}
              {account.status !== 'live' || feed.marketId === null
                ? '—'
                : `${position?.yes ?? '0'} UP / ${position?.no ?? '0'} DOWN`}
            </span>
          </div>
          {trading.reason && <p>{trading.reason}</p>}
        </section>
      }
    />
  )
}

/** The exchange connection strip an NBA round shows: positions read HOME / AWAY. */
function ExchangeConnection({
  config,
  feed,
  account,
  trading,
}: {
  config: RustExchangeConfigProps
  feed: ReturnType<ReturnType<typeof createRustExchangeStore>['getSnapshot']>['feed']
  account: ReturnType<ReturnType<typeof createRustExchangeStore>['getSnapshot']>['account']
  trading: MarketTrading
}) {
  const position = account.positions.find(
    item => feed.marketId !== null && item.market_id === exchangeMarketKey(feed.marketId)
  )
  return (
    <section
      className="flex flex-col gap-sm rounded-md border border-border-subtle bg-surface-raised p-lg text-label"
      aria-label="Exchange connection"
    >
      <strong>Local exchange · {config.series}</strong>
      <output>
        Book: {feed.status} · Account: {account.status} · Round {feed.marketId ?? '—'}:{' '}
        {feed.state ?? 'waiting'}
      </output>
      <div className="flex flex-wrap gap-lg">
        <span>
          Balance: {account.available === null ? '—' : formatExchangeBalance(account.available)}
        </span>
        <span>Locked: {account.locked === null ? '—' : formatExchangeBalance(account.locked)}</span>
        <span>
          Position:{' '}
          {account.status !== 'live' || feed.marketId === null
            ? '—'
            : `${position?.yes ?? '0'} HOME / ${position?.no ?? '0'} AWAY`}
        </span>
      </div>
      {trading.reason && <p>{trading.reason}</p>}
    </section>
  )
}
