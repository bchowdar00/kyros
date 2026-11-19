import { createRustExchangeClient } from '@kyros/api'
import {
  RustAccountsFrameSchema,
  RustBookFrameSchema,
  RustPositionFrameSchema,
  RustPriceFrameSchema,
  type RustOpenOrderRow,
} from '@kyros/domain'

import type { GeminiBookState } from '../types'
import { type AccountState, initialAccount } from './account'
import { type ExchangeBuyQuote, tradingUnavailable } from './buy'
import { allocationTenths, deployUnavailable, type StrategyDeployRequest } from './deploy'
import { type FeedState, initialFeed } from './feed'
import { cancelFailureNotice, deployFailureNotice, orderFailureNotice } from './order-error.mapper'
import { applyRustAccounts, applyRustPosition } from './rust-account'
import { applyRustBook, applyRustRound, type RustRound } from './rust-feed'
import { openDemoSocket } from '@/features/exchange-streams/socket'
import { exchangeSocketUrl } from '@/features/exchange-streams/socket-url'

export interface RustExchangeConfig {
  apiUrl: string
  address: string
  series: string
  /** Called with the latest balance so the app shell can show it. */
  onBalance?: (balanceTenths: number) => void
}

export interface RustExchangeSnapshot {
  feed: FeedState
  account: AccountState
  notice: string
  pending: boolean
  /** A template deploy is in flight. */
  deploying: boolean
  /** The outcome of the last template deploy. */
  deployNotice: string
}

type Socket = ReturnType<typeof openDemoSocket>

/**
 * Same surface as the monofleet `createExchangeStore`, backed by the Rust exchange: the
 * book and round arrive on `/ws/book` and `/ws/price`, the balance and position on
 * `/ws/positions`, the account list with its open orders and trade history on
 * `/ws/accounts`, and orders go over REST. The market screen cannot tell the two apart.
 */
export function createRustExchangeStore(config: RustExchangeConfig) {
  const client = createRustExchangeClient({ baseUrl: config.apiUrl, retry: { attempts: 1 } })
  const listeners = new Set<() => void>()
  let snapshot: RustExchangeSnapshot = {
    feed: initialFeed,
    account: initialAccount,
    notice: '',
    pending: false,
    deploying: false,
    deployNotice: '',
  }
  let round: RustRound | undefined
  let sockets: Socket[] = []
  let closeTimer: ReturnType<typeof setTimeout> | undefined

  const publish = () => {
    for (const listener of listeners) listener()
  }
  const update = (next: Partial<RustExchangeSnapshot>) => {
    snapshot = { ...snapshot, ...next }
    publish()
  }
  const notice = (message: string) => update({ notice: message })
  const socketUrl = (path: string, query: Record<string, string>) =>
    exchangeSocketUrl(config.apiUrl, path, query)

  const applyRound = (next: RustRound | undefined) => {
    round = next
    clearTimeout(closeTimer)
    update({ feed: applyRustRound(snapshot.feed, round, Date.now()) })
    if (round !== undefined && Date.now() < round.closesAt) {
      // The book keeps streaming after close; flip to closed on the clock, not on a frame.
      closeTimer = setTimeout(
        () => update({ feed: applyRustRound(snapshot.feed, round, Date.now()) }),
        round.closesAt - Date.now()
      )
    }
  }

  const start = () => {
    sockets = [
      openDemoSocket(socketUrl('/ws/price', { series: config.series }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustPriceFrameSchema.safeParse(payload)
          if (!parsed.success) return
          const frame = parsed.data
          if (frame.type === 'snapshot') {
            applyRound(
              frame.market_id === null || frame.closes_at_ms === null
                ? undefined
                : { marketId: frame.market_id, closesAt: frame.closes_at_ms }
            )
          } else if (frame.type === 'market_reset' && frame.series === config.series) {
            applyRound({ marketId: frame.market_id, closesAt: frame.closes_at_ms })
            // Settlement moved money and flattened the position; a new snapshot follows.
            update({ notice: 'Round settled. A new round is open.' })
          }
        },
        disconnected: () => applyRound(undefined),
      }),
      openDemoSocket(socketUrl('/ws/book', { series: config.series }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustBookFrameSchema.safeParse(payload)
          if (!parsed.success) return
          update({
            feed: applyRustRound(applyRustBook(snapshot.feed, parsed.data), round, Date.now()),
          })
        },
        disconnected: () =>
          update({ feed: { ...snapshot.feed, status: 'disconnected', seq: null } }),
      }),
      openDemoSocket(
        socketUrl('/ws/positions', { address: config.address, series: config.series }),
        {
          opened: () => undefined,
          message: payload => {
            const parsed = RustPositionFrameSchema.safeParse(payload)
            if (!parsed.success) return
            update({ account: applyRustPosition(snapshot.account, parsed.data) })
            config.onBalance?.(parsed.data.balance_tenths)
          },
          disconnected: () => update({ account: { ...snapshot.account, status: 'disconnected' } }),
        }
      ),
      openDemoSocket(socketUrl('/ws/accounts', { address: config.address }), {
        opened: () => undefined,
        message: payload => {
          const parsed = RustAccountsFrameSchema.safeParse(payload)
          if (!parsed.success) return
          update({ account: applyRustAccounts(snapshot.account, parsed.data, config.series) })
        },
        disconnected: () => undefined,
      }),
    ]
  }

  const stop = () => {
    clearTimeout(closeTimer)
    for (const socket of sockets) socket.close()
    sockets = []
    snapshot = {
      ...snapshot,
      feed: { ...snapshot.feed, status: 'disconnected', seq: null },
      account: { ...snapshot.account, status: 'disconnected' },
    }
  }

  return {
    getSnapshot: () => snapshot,
    subscribe(listener: () => void) {
      listeners.add(listener)
      if (listeners.size === 1) start()
      return () => {
        listeners.delete(listener)
        if (listeners.size === 0) stop()
      }
    },
    /** `geminiBook` is the series' Gemini state as last polled, the same one the screen's reason reads. */
    async buy(quote: ExchangeBuyQuote, geminiBook?: GeminiBookState) {
      if (
        snapshot.pending ||
        tradingUnavailable(snapshot.feed, snapshot.account, geminiBook) !== undefined ||
        snapshot.feed.marketId !== quote.marketId
      )
        return notice('The market changed or is unavailable. Review the current quote.')
      if (
        snapshot.account.available === null ||
        BigInt(quote.reserve) > BigInt(snapshot.account.available)
      )
        return notice('Insufficient available balance.')
      update({ pending: true, notice: 'Order sent. Waiting for the exchange.' })
      const side = quote.side === 'buy' ? 'bid' : 'ask'
      const placed = await client.placeOrder({
        address: config.address,
        series: config.series,
        side,
        price: quote.price,
        qty: Number(quote.qty),
      })
      if (!placed.ok) {
        return update({ pending: false, notice: orderFailureNotice(placed.error) })
      }
      const filled = placed.data.fills.reduce((total, fill) => total + fill.qty, 0)
      const resting = placed.data.resting_qty
      // The resting order itself arrives on the next accounts frame; nothing is inferred here.
      update({
        pending: false,
        notice:
          filled > 0
            ? `Filled ${filled} lots${resting > 0 ? `, ${resting} resting` : ''}.`
            : 'Order resting on the book.',
      })
    },
    /** Deploy one side of a public template; true once the exchange has funded and started it. */
    async deployTemplate(request: StrategyDeployRequest): Promise<boolean> {
      const blocked = deployUnavailable(snapshot.account, snapshot.deploying)
      if (blocked !== undefined) {
        update({ deployNotice: blocked })
        return false
      }
      update({ deploying: true, deployNotice: 'Deploying. Waiting for the exchange.' })
      const deployed = await client.deployTemplate(
        config.address,
        request.templateId,
        {
          allocation_tenths: allocationTenths(request.allocationCents),
          inverse: request.inverse,
          series: config.series,
        },
        crypto.randomUUID()
      )
      if (!deployed.ok) {
        update({ deploying: false, deployNotice: deployFailureNotice(deployed.error) })
        return false
      }
      // The new sub-account's row arrives on the next accounts frame.
      const leg = deployed.data.runtime?.current_leg
      update({
        deploying: false,
        deployNotice: `Deployed ${deployed.data.strategy.name}${leg === undefined ? '' : `, now ${leg}`}.`,
      })
      return true
    },
    /** Cancels as the owning account: a strategy's order is cancelled as that strategy. */
    async cancel(order: RustOpenOrderRow) {
      if (snapshot.account.status !== 'live') return notice('Reconnect before cancelling.')
      const cancelled = await client.cancelOrder(order.order_id, order.account, order.series)
      if (!cancelled.ok) return notice(cancelFailureNotice(cancelled.error))
      // The accounts frame drops the row on the book broadcast the cancel triggers.
      notice('Order cancelled.')
    },
  }
}
export type RustExchangeStore = ReturnType<typeof createRustExchangeStore>
