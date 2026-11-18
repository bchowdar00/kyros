import * as z from 'zod/mini'

export const ExchangeUint = z.string().check(
  z.maxLength(20),
  z.regex(/^(0|[1-9]\d*)$/),
  z.refine(value => value.length < 20 || (value.length === 20 && value <= '18446744073709551615'))
)
const Price = z.int().check(z.minimum(1), z.maximum(999))
const Address = z.string().check(z.length(40), z.regex(/^[0-9a-f]+$/))
const MarketKey = z.string().check(z.length(64), z.regex(/^[0-9a-f]+$/))
const State = z.nullable(z.enum(['created', 'open', 'closed', 'settled']))
const Step = z.nullable(z.int().check(z.minimum(1), z.maximum(999)))
const Side = z.enum(['buy', 'sell'])
const Level = z.object({ price: Price, qty: ExchangeUint, orders: z.int() })
const Trade = z.object({
  book_seq: ExchangeUint,
  fill_index: z.int(),
  price: Price,
  qty: ExchangeUint,
  taker_side: Side,
})
const common = { seq: ExchangeUint, market_id: z.nullable(ExchangeUint) }
export const ExchangeFeedSchema = z.union([
  z.object({
    ...common,
    type: z.literal('snapshot'),
    state: State,
    tick_step: Step,
    bids: z.array(Level),
    asks: z.array(Level),
    trades: z.array(Trade),
    closes_at_ms: z.nullable(ExchangeUint),
  }),
  z.object({
    ...common,
    type: z.literal('delta'),
    kind: z.literal('book'),
    side: z.enum(['bid', 'ask']),
    price: Price,
    qty: ExchangeUint,
    orders: z.int(),
  }),
  z.object({ ...common, type: z.literal('delta'), kind: z.literal('trade'), ...Trade.shape }),
  z.object({
    ...common,
    type: z.literal('delta'),
    kind: z.literal('lifecycle'),
    state: State,
    tick_step: Step,
  }),
  z.object({
    ...common,
    type: z.literal('delta'),
    kind: z.literal('tick_step_change'),
    new_tick_step: Step,
  }),
  z.object({ ...common, type: z.literal('delta'), kind: z.literal('stats') }),
  z.object({ ...common, type: z.literal('stale') }),
])
export type ExchangeFeed = z.infer<typeof ExchangeFeedSchema>
export type ExchangeLevel = z.infer<typeof Level>
export const ExchangeAuthSchema = z.object({
  version: z.literal(6),
  type: z.literal('auth'),
  body: z.object({
    account: ExchangeUint,
    expires_at: ExchangeUint,
    key_id: z.int(),
    sig: z.string().check(z.length(128), z.regex(/^[0-9a-f]+$/)),
    cancel_on_disconnect_ms: z.int(),
  }),
})
const balance = {
  balance_seq: ExchangeUint,
  available: ExchangeUint,
  locked: ExchangeUint,
  pending_finality: ExchangeUint,
}
const order = {
  order_id: ExchangeUint,
  market_id: MarketKey,
  side: Side,
  price: z.int(),
  qty: ExchangeUint,
}
export const ExchangeGatewaySchema = z.union([
  z.object({
    type: z.literal('market_state'),
    body: z.object({
      market_id: MarketKey,
      fee_rate_bps: z.int().check(z.minimum(0), z.maximum(1000)),
    }),
  }),
  z.object({
    type: z.enum([
      'auth_ok',
      'snapshot_begin',
      'snapshot_end',
      'order_submitted',
      'fill_settled',
      'market_settled',
    ]),
    body: z.object({}),
  }),
  z.object({
    type: z.enum(['auth_failed', 'bad_request']),
    body: z.object({ reason: z.string().check(z.maxLength(200)) }),
  }),
  z.object({ type: z.enum(['balance', 'snapshot_balance']), body: z.object(balance) }),
  z.object({
    type: z.enum(['snapshot_order', 'order_accepted']),
    body: z.object({ ...order, remaining: z.optional(ExchangeUint) }),
  }),
  z.object({
    type: z.literal('fill'),
    body: z.object({
      fee: ExchangeUint,
      book_seq: ExchangeUint,
      fill_index: z.int(),
      order_id: ExchangeUint,
      market_id: MarketKey,
      qty: ExchangeUint,
      price: Price,
      remaining: ExchangeUint,
      position_yes: ExchangeUint,
      position_no: ExchangeUint,
    }),
  }),
  z.object({
    type: z.literal('snapshot_position'),
    body: z.object({ market_id: MarketKey, yes_lots: ExchangeUint, no_lots: ExchangeUint }),
  }),
  z.object({
    type: z.enum(['cancel_done', 'order_expired']),
    body: z.object({ order_id: ExchangeUint }),
  }),
  z.object({
    type: z.literal('order_rejected'),
    body: z.object({
      order_id: ExchangeUint,
      modify_seq: ExchangeUint,
      reason: z.string().check(z.maxLength(200)),
    }),
  }),
])
export type ExchangeGateway = z.infer<typeof ExchangeGatewaySchema>

export function exchangeMarketKey(id: string): string {
  const parsed = ExchangeUint.parse(id)
  if (BigInt(parsed) > 4294967295n) throw new Error('Market id exceeds u32')
  return BigInt(parsed).toString(16).padStart(64, '0')
}

export interface ExchangeOrderInput {
  account: string
  marketId: string
  side: 'buy' | 'sell'
  price: number
  qty: string
  salt: string
  validUntil: string
  kind?: 'gtc' | 'ioc'
}

/** Demo orders use monofleet's placeholder signature; never use with funded venues. */
export function encodeDemoOrder(input: ExchangeOrderInput): string {
  Address.parse(input.account)
  Price.parse(input.price)
  ExchangeUint.parse(input.qty)
  ExchangeUint.parse(input.salt)
  ExchangeUint.parse(input.validUntil)
  if (BigInt(input.qty) === 0n) throw new Error('Quantity must be positive')
  return JSON.stringify({
    version: 6,
    type: 'new_order',
    body: {
      salt: input.salt,
      valid_until: input.validUntil,
      account: input.account,
      market_id: exchangeMarketKey(input.marketId),
      side: input.side,
      kind: input.kind ?? 'gtc',
      reduce_only: false,
      price: input.price,
      qty: input.qty,
      expiry: '0',
      sig: '0'.repeat(130),
    },
  })
}

export function encodeDemoCancel(account: string, marketKey: string, orderId: string): string {
  Address.parse(account)
  MarketKey.parse(marketKey)
  ExchangeUint.parse(orderId)
  return JSON.stringify({
    version: 6,
    type: 'cancel_order',
    body: { account, market_id: marketKey, order_id: orderId },
  })
}
