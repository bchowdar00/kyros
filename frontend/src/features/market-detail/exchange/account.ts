import type {
  ExchangeGateway,
  RustAccountRow,
  RustOpenOrderRow,
  RustUserTradeRow,
} from '@kyros/domain'

export interface DemoOrder {
  order_id: string
  market_id: string
  side: 'buy' | 'sell'
  price: number
  remaining: string
}
export interface Position {
  market_id: string
  yes: string
  no: string
}
export interface AccountFill {
  key: string
  marketId: string
  price: number
  qty: string
  fee: string
}
export interface AccountState {
  status: 'connecting' | 'syncing' | 'live' | 'disconnected' | 'expired'
  balanceSeq: string | null
  available: string | null
  locked: string | null
  orders: DemoOrder[]
  positions: Position[]
  fees: Record<string, number>
  fills: AccountFill[]
  /**
   * The Rust exchange's `/ws/accounts` view: the main account first, then every live
   * strategy, with every resting order they hold and their merged order log (newest
   * first). Empty until the first accounts frame; the monofleet gateway never fills them.
   */
  accounts: RustAccountRow[]
  openOrders: RustOpenOrderRow[]
  trades: RustUserTradeRow[]
}
export const initialAccount: AccountState = {
  status: 'connecting',
  balanceSeq: null,
  available: null,
  locked: null,
  orders: [],
  positions: [],
  fees: {},
  fills: [],
  accounts: [],
  openOrders: [],
  trades: [],
}
export function applyAccount(previous: AccountState, frame: ExchangeGateway): AccountState {
  const next = { ...previous }
  if (frame.type === 'market_state')
    return { ...next, fees: { ...previous.fees, [frame.body.market_id]: frame.body.fee_rate_bps } }
  if (frame.type === 'auth_ok') return { ...next, status: 'syncing' }
  if (frame.type === 'auth_failed') return { ...next, status: 'expired' }
  if (frame.type === 'snapshot_begin')
    return { ...initialAccount, status: 'syncing', fills: previous.fills }
  if (frame.type === 'snapshot_end') return { ...next, status: 'live' }
  if (frame.type === 'balance' || frame.type === 'snapshot_balance') {
    if (
      previous.balanceSeq !== null &&
      BigInt(frame.body.balance_seq) <= BigInt(previous.balanceSeq)
    )
      return previous
    return {
      ...next,
      balanceSeq: frame.body.balance_seq,
      available: frame.body.available,
      locked: frame.body.locked,
    }
  }
  if (frame.type === 'snapshot_order' || frame.type === 'order_accepted') {
    const order = { ...frame.body, remaining: frame.body.remaining ?? frame.body.qty }
    next.orders = [...previous.orders.filter(item => item.order_id !== order.order_id), order]
  }
  if (frame.type === 'fill') {
    const body = frame.body
    const key = `${body.order_id}:${body.book_seq}:${body.fill_index}`
    next.fills = [
      { key, marketId: body.market_id, price: body.price, qty: body.qty, fee: body.fee },
      ...previous.fills.filter(fill => fill.key !== key),
    ].slice(0, 20)
    next.orders = previous.orders.flatMap(order =>
      order.order_id !== body.order_id
        ? [order]
        : body.remaining === '0'
          ? []
          : [{ ...order, remaining: body.remaining }]
    )
    next.positions = [
      ...previous.positions.filter(position => position.market_id !== body.market_id),
      { market_id: body.market_id, yes: body.position_yes, no: body.position_no },
    ]
  }
  if (frame.type === 'snapshot_position') {
    const body = frame.body
    next.positions = [
      ...previous.positions.filter(position => position.market_id !== body.market_id),
      { market_id: body.market_id, yes: body.yes_lots, no: body.no_lots },
    ]
  }
  if (
    frame.type === 'cancel_done' ||
    frame.type === 'order_expired' ||
    (frame.type === 'order_rejected' && frame.body.modify_seq === '0')
  ) {
    const orderId = frame.body.order_id
    next.orders = previous.orders.filter(order => order.order_id !== orderId)
  }
  return next
}
