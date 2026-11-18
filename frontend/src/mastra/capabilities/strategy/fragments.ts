import type { AuthoringDoItem, AuthoringTrigger } from './authoring-schema'
import type { ActionKind } from './plan'
import { BTC_SIDES, type Sides, labelOf } from './sides'

/**
 * The smallest correct pieces a document is assembled from, one per plan action kind
 * plus the triggers every shape needs. The writer composes these instead of editing a
 * whole example, so it cannot inherit another strategy's leg count. Every spelling
 * here is what the compiler accepts and the runtime charges correctly: takes are
 * immediate limits a cent through the book, sizes are dollars over the price paid,
 * flattens are reduce-only on the side the position actually has.
 *
 * Sides are reasoned about in the quoted frame (`up` is the quoted side, `down` its
 * complement) and written with the market's own labels: `sides` says what the series
 * calls them, and defaults to BTC's up and down.
 */

export const MAX_SIZE = 'envelope.max_order_size'
export const FLATTEN_SIZE =
  'clamp(abs(platform.position.size), 0, envelope.max_position_per_market)'
export const FLATTEN_MAX_SIZE = 'envelope.max_position_per_market'
const BAND = { price_band_lo: '0.01', price_band_hi: '0.99' } as const

/** The price an immediate order pays: a cent through the book on its side. */
export function takePrice(outcome: 'up' | 'down') {
  return outcome === 'up' ? 'signal.book.ask + 0.01' : '1 - signal.book.bid + 0.01'
}

/** Dollars of the round budget over the price paid, floored to whole contracts. */
export function dollarSize(share: string, price: string) {
  return `clamp(floor(envelope.round_budget * ${share} / (${price})), 0, ${MAX_SIZE})`
}

const EMPTY_ITEM: AuthoringDoItem = {
  kind: 'statement',
  statement: null,
  instrument: null,
  outcome: null,
  side: null,
  order_type: null,
  tif: null,
  price: null,
  price_band_lo: null,
  price_band_hi: null,
  size: null,
  max_size: null,
  tag: null,
  post_only: null,
  expire_after: null,
  reduce_only: null,
  on_fill: null,
  on_partial: null,
  on_reject: null,
  on_cancel_ack: null,
  on_expiry: null,
}

export function item(fields: Partial<AuthoringDoItem>): AuthoringDoItem {
  return { ...EMPTY_ITEM, ...fields }
}

export function statement(text: string): AuthoringDoItem {
  return item({ kind: 'statement', statement: text })
}

/** Where a take goes when it gets nothing (expiry) or is refused (reject). */
export interface WalkBack {
  reject: string
  expiry: string
}

/** Take the market: an immediate limit a cent through the book, sized in dollars. */
export function take(
  outcome: 'up' | 'down',
  share: string,
  tag: string,
  back: WalkBack,
  sides: Sides = BTC_SIDES
) {
  return item({
    kind: 'place',
    instrument: 'market',
    outcome: labelOf(sides, outcome),
    side: 'buy',
    order_type: 'limit',
    tif: 'ioc',
    price: `clamp(${takePrice(outcome)}, 0.01, 0.99)`,
    ...BAND,
    size: dollarSize(share, takePrice(outcome)),
    max_size: MAX_SIZE,
    tag,
    on_reject: back.reject,
    on_expiry: back.expiry,
  })
}

/** Rest a bid on the quoted side a cent above the best bid. */
export function restBid(
  share: string,
  tag: string,
  onFill: string | null,
  sides: Sides = BTC_SIDES
) {
  return item({
    kind: 'place',
    instrument: 'market',
    outcome: sides.quoted,
    side: 'buy',
    order_type: 'limit',
    tif: 'gtc',
    price: 'clamp(signal.book.bid + 0.01, 0.01, 0.99)',
    ...BAND,
    size: dollarSize(share, 'signal.book.bid + 0.01'),
    max_size: MAX_SIZE,
    tag,
    on_fill: onFill,
  })
}

/** Rest an ask on the quoted side a cent under the best ask; a sell's collateral is its complement. */
export function restAsk(
  share: string,
  tag: string,
  onFill: string | null,
  sides: Sides = BTC_SIDES
) {
  return item({
    kind: 'place',
    instrument: 'market',
    outcome: sides.quoted,
    side: 'sell',
    order_type: 'limit',
    tif: 'gtc',
    price: 'clamp(signal.book.ask - 0.01, 0.01, 0.99)',
    ...BAND,
    size: dollarSize(share, '1.01 - signal.book.ask'),
    max_size: MAX_SIZE,
    tag,
    on_fill: onFill,
  })
}

/** Re-quote a resting tag in place; self-limiting, so it needs no pacing. */
export function requote(side: 'bid' | 'ask', share: string, tag: string, sides: Sides = BTC_SIDES) {
  const resting =
    side === 'bid' ? restBid(share, tag, null, sides) : restAsk(share, tag, null, sides)
  return item({ ...resting, kind: 'cancel_replace', on_fill: null })
}

export function cancel(tag: string) {
  return item({ kind: 'cancel', tag })
}

/** Sell the whole long at market, reduce-only so a double fire cannot go short. */
export function flattenLong(tag: string, sides: Sides = BTC_SIDES) {
  return item({
    kind: 'place',
    instrument: 'market',
    outcome: sides.quoted,
    side: 'sell',
    order_type: 'market',
    tif: 'ioc',
    reduce_only: true,
    size: FLATTEN_SIZE,
    max_size: FLATTEN_MAX_SIZE,
    tag,
  })
}

/** Buy back the whole short at market, reduce-only. The other side is a short on the quoted one. */
export function flattenShort(tag: string, sides: Sides = BTC_SIDES) {
  return item({ ...flattenLong(tag, sides), side: 'buy' })
}

/** Reach flat with one market order whichever side is held. */
export function targetFlat(tag: string, sides: Sides = BTC_SIDES) {
  return item({
    kind: 'target_position',
    instrument: 'market',
    outcome: sides.quoted,
    reduce_only: true,
    size: 'clamp(0, 0, envelope.max_position_per_market)',
    max_size: MAX_SIZE,
    tag,
  })
}

/**
 * Flip: flatten the held side, then take the other side with the dollars just
 * released. Both sizes are evaluated before either order is placed, so the second
 * still sees the pre-flatten position. A long released `size × avg_cost`; a short
 * released `size × (1 - avg_cost)`.
 */
export function flip(
  from: 'up' | 'down',
  flattenTag: string,
  takeTag: string,
  back: WalkBack,
  sides: Sides = BTC_SIDES
) {
  const to = from === 'up' ? 'down' : 'up'
  const released =
    from === 'up'
      ? 'abs(platform.position.size) * platform.position.avg_cost'
      : 'abs(platform.position.size) * (1 - platform.position.avg_cost)'
  const flatten = from === 'up' ? flattenLong(flattenTag, sides) : flattenShort(flattenTag, sides)
  const buy = item({
    ...take(to, '1', takeTag, back, sides),
    size: `clamp(floor(${released} / (${takePrice(to)})), 0, ${MAX_SIZE})`,
  })
  return [flatten, buy]
}

// ───────────────────────────────── triggers ─────────────────────────────────

const EMPTY_TRIGGER: AuthoringTrigger = {
  name: '',
  source: 'condition',
  group: 'single',
  leaves: [],
  instrument: null,
  duration: null,
  fire: 'if_true',
  rearm: 'immediate',
  cooldown: null,
}

export function trigger(fields: Partial<AuthoringTrigger> & { name: string }): AuthoringTrigger {
  return { ...EMPTY_TRIGGER, ...fields }
}

export function leaf(
  fields: Partial<AuthoringTrigger['leaves'][number]>
): AuthoringTrigger['leaves'][number] {
  return {
    signal: null,
    field: null,
    expr: null,
    comparator: null,
    value: null,
    value_kind: null,
    ...fields,
  }
}

/** "When the field crosses the level": fires on the rising edge, once per crossing. */
export function crossing(
  name: string,
  field: string,
  comparator: '>' | '<' | '>=' | '<=',
  value: string
) {
  return trigger({
    name,
    leaves: [leaf({ signal: 'book', field, comparator, value, value_kind: 'literal' })],
    fire: 'on_crossing',
    rearm: 'fresh_crossing',
  })
}

/** Wake on every book publish; the real condition lives in the guard. */
export const bookTick = trigger({ name: 'book_tick', leaves: [leaf({ signal: 'book' })] })

/** N before the close, once per round. */
export function stopTimer(name: string, remaining: string) {
  return trigger({
    name,
    source: 'window_clock',
    instrument: 'market',
    duration: remaining,
    fire: 'on_crossing',
    rearm: 'fresh_crossing',
  })
}

export function interval(name: string, every: string) {
  return trigger({
    name,
    source: 'interval',
    duration: every,
    fire: 'on_crossing',
    rearm: 'immediate',
  })
}

// ──────────────────────────── the catalogue for prompts ────────────────────────────

const SHARE = '0.25'
const BACK: WalkBack = { reject: 'hunting', expiry: 'hunting' }

export type FragmentKind = Exclude<ActionKind, 'record' | 'none'>
export type FragmentCatalogue = Record<FragmentKind, { note: string; items: AuthoringDoItem[] }>

/** One worked spelling per action kind in the market's labels, for the kinds a plan uses. */
export function fragments(sides: Sides): FragmentCatalogue {
  const { quoted, other } = sides
  return {
    take_up: {
      note: `Buy ${quoted} (the quoted side) at once; expiry and reject walk back to the hunting leg.`,
      items: [take('up', SHARE, 'entry', BACK, sides)],
    },
    take_down: {
      note: `Buy ${other} at once; the price is the complement of the ${quoted} bid.`,
      items: [take('down', SHARE, 'entry', BACK, sides)],
    },
    rest_bid: {
      note: `Rest a bid on ${quoted}; on_fill moves to the holding leg.`,
      items: [restBid(SHARE, 'bid', 'holding', sides)],
    },
    rest_ask: {
      note: `Rest an ask on ${quoted}; the collateral is the complement of the price.`,
      items: [restAsk(SHARE, 'ask', 'holding', sides)],
    },
    requote: {
      note: 'Follow the book in place; no pacing needed.',
      items: [requote('bid', SHARE, 'bid', sides)],
    },
    cancel: { note: 'Pull a resting tag.', items: [cancel('bid')] },
    flatten: {
      note: `Sell the whole long (position > 0, holding ${quoted}) or buy back the whole short (position < 0, holding ${other}), picked by the side the leg holds; or target flat when the side is not known.`,
      items: [flattenLong('exit', sides), flattenShort('exit', sides), targetFlat('flat', sides)],
    },
    flip: {
      note: `From holding ${quoted}: flatten the long, then buy ${other} with the dollars released. From holding ${other}, swap the sides.`,
      items: flip('up', 'flatten', 'flip', BACK, sides),
    },
  }
}

/** The catalogue in BTC's labels. */
export const FRAGMENTS: FragmentCatalogue = fragments(BTC_SIDES)

export function renderFragments(kinds: readonly ActionKind[], sides: Sides = BTC_SIDES) {
  const catalogue = fragments(sides)
  const wanted = [...new Set(kinds)].filter((kind): kind is FragmentKind => kind in catalogue)
  return wanted
    .map(kind => `### ${kind}\n${catalogue[kind].note}\n${JSON.stringify(catalogue[kind].items)}`)
    .join('\n\n')
}
