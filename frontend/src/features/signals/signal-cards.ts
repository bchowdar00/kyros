import type { RustSignal, RustSignalCatalogue } from '@kyros/domain'

import { type PinKey, pinKey } from './signal-catalogue'
import { formatSignalValue, isNumericField } from './signal-value-format'
import type { MarketSignal } from '@/features/market-detail/types'

/** One card in Browse All: a single field of a single signal, with a value. */
export interface SignalFieldCard {
  pin: PinKey
  signalId: string
  label: string
  value: string
  /** A number a trace can follow, or a state shown as a word. */
  kind: 'number' | 'state'
  /** The signal's two sentences, shown on its block and as the row's tooltip. */
  summary: string
  /** The field's own two sentences, for the detail view; absent until authored. */
  description?: string
  signalName: string
  venue: string
}

/** One signal's rows under a venue: a title, a line about it, and its numbers. */
export interface SignalBlock {
  signalId: string
  title: string
  summary: string
  cards: SignalFieldCard[]
}

export interface SignalCardGroup {
  venue: string
  /** Rows across every block, for the header count. */
  count: number
  blocks: SignalBlock[]
}

export const THIS_MARKET = 'This market'

/** Readable group name per id segment; the lane name collapses spot venues, so ids are used. */
const VENUE_NAMES: Readonly<Record<string, string>> = {
  binance: 'Binance',
  bybit: 'Bybit',
  coinbase: 'Coinbase',
  kraken: 'Kraken',
  external: 'Blended',
  cross: 'Cross-venue',
  multi: 'Cross-venue',
  pyth: 'Oracles',
  chainlink: 'Oracles',
  cfbrti: 'Oracles',
  kalshi: 'Kalshi',
  polymarket: 'Polymarket',
}

/** Group order in the panel; anything unlisted follows alphabetically. */
const VENUE_ORDER = [
  THIS_MARKET,
  'Oracles',
  'Blended',
  'Coinbase',
  'Kraken',
  'Binance',
  'Bybit',
  'Cross-venue',
  'Polymarket',
  'Kalshi',
]

/**
 * The venue a signal belongs to. Series internals are the market's own; lane signals carry
 * the venue as the segment after the underlying (`btc.coinbase.rsi.v1`), and the spot feed
 * carries it first (`pyth.btc_usd.spot.v1`).
 */
export function venueOf(signal: RustSignal): string {
  if (signal.scope.kind === 'series') return THIS_MARKET
  const underlying = signal.scope.kind === 'underlying' ? signal.scope.underlying : undefined
  const segment = signal.signal_id.split('.').find(part => part !== underlying) ?? ''
  const known = VENUE_NAMES[segment]
  if (known !== undefined) return known
  return segment === '' ? 'Other' : segment.charAt(0).toUpperCase() + segment.slice(1)
}

/** "Venue price" reads on as "Polymarket venue price"; an acronym like "RSI" keeps its case. */
function runOn(venue: string, text: string): string {
  const lowered = /^[A-Z][a-z]/.test(text) ? text.charAt(0).toLowerCase() + text.slice(1) : text
  return `${venue} ${lowered}`
}

/** The shape of a signal id: `btc.coinbase.rsi.v1`, `btc-5m.current.stats.v1`. */
const SIGNAL_ID = /^[a-z0-9-]+\.[a-z0-9_-]+\.[a-z0-9_]+\.v\d+$/

/**
 * The English name for a signal id, when the id is one the catalogue lists: "Coinbase
 * momentum" for `btc.coinbase.micro_momentum.v1`. Authored content from the model names
 * signals by id; a reader should see the name. The name is the server's copy, so without
 * a catalogue (a chat not connected to an exchange) the id stands.
 */
export function signalDisplayName(
  text: string,
  catalogue: RustSignalCatalogue | null | undefined
): string | undefined {
  if (!SIGNAL_ID.test(text)) return undefined
  const signal = catalogue?.signals.find(entry => entry.signal_id === text)
  if (signal === undefined) return undefined
  const venue = venueOf(signal)
  return venue === THIS_MARKET ? signal.name : runOn(venue, signal.name)
}

/**
 * Where a pinned number comes from, as the strip card's tooltip: the venue and the signal's
 * name ("Polymarket venue price"); the market's own signals need only the name.
 */
export function pinnedCardContext(signal: RustSignal): string {
  const venue = venueOf(signal)
  return venue === THIS_MARKET ? signal.name : runOn(venue, signal.name)
}

/** The strip card's name: the field's label, led by its venue unless it is the market's own. */
export function pinnedCardLabel(signal: RustSignal, label: string): string {
  const venue = venueOf(signal)
  return venue === THIS_MARKET ? label : runOn(venue, label)
}

/** Resolve pins against the catalogue, in pin order, skipping fields no longer listed. */
export function pinnedSignalCards(
  catalogue: RustSignalCatalogue,
  pins: readonly PinKey[]
): MarketSignal[] {
  const cards: MarketSignal[] = []
  for (const pin of pins) {
    for (const signal of catalogue.signals) {
      if (!pin.startsWith(`${signal.signal_id}:`)) continue
      const field = signal.fields.find(f => pinKey(signal.signal_id, f.name) === pin)
      if (field === undefined) continue
      cards.push({
        id: pin,
        context: pinnedCardContext(signal),
        label: pinnedCardLabel(signal, field.label),
        value: formatSignalValue(signal, field),
      })
    }
  }
  return cards
}

export interface SignalFieldCardsOptions {
  /** Include the plumbing fields the server marks as not headline (timestamps, ages, parameters). */
  showAll?: boolean
}

/**
 * Every field of a ready signal that carries a value, in registry order; headline fields
 * only by default. A signal still warming up shows nothing rather than a placeholder.
 */
export function signalFieldCards(
  catalogue: RustSignalCatalogue,
  { showAll = false }: SignalFieldCardsOptions = {}
): SignalFieldCard[] {
  const cards: SignalFieldCard[] = []
  for (const signal of catalogue.signals) {
    if (signal.quality.state !== 'ready') continue
    const venue = venueOf(signal)
    for (const field of signal.fields) {
      if (field.value === undefined) continue
      if (!showAll && !field.headline) continue
      cards.push({
        pin: pinKey(signal.signal_id, field.name),
        signalId: signal.signal_id,
        label: field.label,
        value: formatSignalValue(signal, field),
        kind: isNumericField(field) ? 'number' : 'state',
        summary: signal.description,
        ...(field.description === undefined ? {} : { description: field.description }),
        signalName: signal.name,
        venue,
      })
    }
  }
  return cards
}

function venueRank(venue: string): number {
  const index = VENUE_ORDER.indexOf(venue)
  return index === -1 ? VENUE_ORDER.length : index
}

/**
 * Cards bucketed by venue and, inside each venue, by signal, in panel order. Block order
 * and card order within a block follow the registry.
 */
export function groupCardsByVenue(cards: readonly SignalFieldCard[]): SignalCardGroup[] {
  const groups = new Map<string, Map<string, SignalBlock>>()
  for (const card of cards) {
    let blocks = groups.get(card.venue)
    if (blocks === undefined) {
      blocks = new Map()
      groups.set(card.venue, blocks)
    }
    const block = blocks.get(card.signalId)
    if (block === undefined) {
      blocks.set(card.signalId, {
        signalId: card.signalId,
        title: card.signalName,
        summary: card.summary,
        cards: [card],
      })
    } else {
      block.cards.push(card)
    }
  }
  return [...groups]
    .map(([venue, blocks]) => {
      const list = [...blocks.values()]
      return { venue, count: list.reduce((n, block) => n + block.cards.length, 0), blocks: list }
    })
    .toSorted((a, b) => venueRank(a.venue) - venueRank(b.venue) || a.venue.localeCompare(b.venue))
}
