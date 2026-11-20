import * as z from 'zod/mini'

/**
 * Kalshi's public `/events` list (`GET /trade-api/v2/events?series_ticker=…`), turned into the
 * games the home page shows for the leagues Gemini does not list. Only the fields read here are
 * described; a market or event that does not fit is skipped, never guessed at. Read-only and
 * public; nothing here touches an account, and the prices are shown to watch, not to trade.
 */

/**
 * Leagues Kalshi prices and Gemini's board did not carry when it was read in full on
 * 2026-11-20. The page checks again each time it has Gemini's games ([`withoutListed`]), so a
 * league Gemini starts listing stops being shown as missing. `names` are the words, in lower
 * case, by which a board might list the league.
 */
export const VENUE_LEAGUES = [
  { label: 'Bundesliga', names: ['bundesliga'], series: 'KXBUNDESLIGAGAME' },
  { label: 'Serie A', names: ['serie a'], series: 'KXSERIEAGAME' },
  { label: 'Ligue 1', names: ['ligue 1'], series: 'KXLIGUE1GAME' },
  { label: 'MLS', names: ['mls', 'major league soccer'], series: 'KXMLSGAME' },
  { label: 'UFC', names: ['ufc', 'mma'], series: 'KXUFCFIGHT' },
] as const

/** Games shown per league: one row of the board at its widest. */
export const PER_LEAGUE = 5

export interface VenueOutcome {
  label: string
  percent: number
}
export interface VenueGame {
  id: string
  title: string
  when: string | undefined
  outcomes: VenueOutcome[]
  volume24h: number
}
export interface VenueLeagueBoard {
  label: string
  games: VenueGame[]
}
export interface VenueBoard {
  leagues: VenueLeagueBoard[]
}

const PERCENT = 100
/** Prices sit strictly inside a dollar: a book quoting 0 or 1 is not a price for either side. */
const FLOOR = 0
const CEILING = 1
/** An outcome needs a price from each side of a question with two sides. */
const MIN_OUTCOMES = 2
/** The widest book, in cents, whose middle still says something about the price. */
const WIDEST_BOOK_CENTS = 10

const MarketSchema = z.object({
  market_type: z.string(),
  status: z.string(),
  ticker: z.string(),
  volume_24h_fp: z.optional(z.string()),
  yes_ask_dollars: z.optional(z.string()),
  yes_bid_dollars: z.optional(z.string()),
  yes_sub_title: z.string(),
})

const EventSchema = z.object({
  event_ticker: z.string(),
  markets: z.array(MarketSchema),
  sub_title: z.optional(z.string()),
  title: z.string(),
})

const BodySchema = z.object({ events: z.array(z.unknown()) })

type VenueEvent = z.infer<typeof EventSchema>

/** A kept outcome and the 24-hour volume of the market it came from, summed but never shown. */
interface RankedOutcome {
  label: string
  percent: number
  volume: number
  /** True when the bid and the ask sit too far apart for their middle to be a price. */
  wide: boolean
}

/** A dollar price string as a number, or undefined when it is not a finite number. */
function toNumber(value: string | undefined): number | undefined {
  if (value === undefined) return undefined
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : undefined
}

/** Rounds a fraction of a dollar to a whole-and-tenths percentage. */
function toPercent(mid: number): number {
  return Math.round(mid * PERCENT * 10) / 10
}

/** The text inside the trailing parentheses of a subtitle, undefined when there is none. */
function whenOf(subTitle: string | undefined): string | undefined {
  if (subTitle === undefined) return undefined
  const match = /\(([^)]*)\)\s*$/.exec(subTitle)
  return match === null ? undefined : (match[1] ?? undefined)
}

/** The priced outcome a market gives, or undefined when the market is not one to show. */
function outcomeOf(market: z.infer<typeof MarketSchema>): RankedOutcome | undefined {
  if (market.status !== 'active' || market.market_type !== 'binary') return undefined
  const bid = toNumber(market.yes_bid_dollars)
  const ask = toNumber(market.yes_ask_dollars)
  if (bid === undefined || ask === undefined) return undefined
  if (!(FLOOR < bid && bid < ask && ask < CEILING)) return undefined
  return {
    label: market.yes_sub_title,
    percent: toPercent((bid + ask) / 2),
    volume: toNumber(market.volume_24h_fp) ?? 0,
    wide: Math.round((ask - bid) * PERCENT) > WIDEST_BOOK_CENTS,
  }
}

/** One game's outcomes, ordered by the title's names, then Tie, then anything else as given. */
function gameOf(event: VenueEvent): VenueGame | undefined {
  // A fight's title opens with the name of its card ("Fight Night: A vs B").
  const names = event.title.replace(/^[^:]+:\s*/, '').split(' vs ')
  const rankOf = (label: string): number => {
    const named = names.indexOf(label)
    if (named !== -1) return named
    return label === 'Tie' ? names.length : names.length + 1
  }

  const outcomes = event.markets
    .map(outcomeOf)
    .filter((outcome): outcome is RankedOutcome => outcome !== undefined)
    .map((outcome, order) => ({ order, outcome }))
    .toSorted((a, b) => rankOf(a.outcome.label) - rankOf(b.outcome.label) || a.order - b.order)
    .map(entry => entry.outcome)

  if (outcomes.length < MIN_OUTCOMES) return undefined
  // One side without a real price makes the whole game's percentages misleading.
  if (outcomes.some(outcome => outcome.wide)) return undefined

  return {
    id: event.event_ticker,
    outcomes: outcomes.map(outcome => ({ label: outcome.label, percent: outcome.percent })),
    title: event.title,
    volume24h: outcomes.reduce((sum, outcome) => sum + outcome.volume, 0),
    when: whenOf(event.sub_title),
  }
}

/**
 * The games in a Kalshi `/events` body, each with its outcomes and prices, ordered by 24-hour
 * volume and capped at `PER_LEAGUE`. A body that is not the expected shape gives `[]`.
 */
export function toVenueGames(body: unknown): VenueGame[] {
  const parsed = z.safeParse(BodySchema, body)
  if (!parsed.success) return []

  const games: VenueGame[] = []
  for (const entry of parsed.data.events) {
    const event = z.safeParse(EventSchema, entry)
    if (!event.success) continue
    const game = gameOf(event.data)
    if (game !== undefined) games.push(game)
  }

  return games
    .toSorted((a, b) => b.volume24h - a.volume24h || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
    .slice(0, PER_LEAGUE)
}

/**
 * The board without any league Gemini lists itself, under any of the league's names and
 * whatever the case; nothing when none is left. What the section says of these games, that
 * Gemini does not list them, has to be true of each one shown.
 */
export function withoutListed(
  board: VenueBoard,
  listed: readonly string[]
): VenueBoard | undefined {
  // Each listed name as plain words with a space either side, so only whole words match.
  const spoken = listed.map(name => ` ${name.toLowerCase().replaceAll(/[^a-z0-9]+/g, ' ')} `)
  const leagues = board.leagues.filter(league => {
    const known = VENUE_LEAGUES.find(each => each.label === league.label)
    const names = known === undefined ? [league.label.toLowerCase()] : known.names
    return !names.some(name => spoken.some(words => words.includes(` ${name} `)))
  })
  return leagues.length === 0 ? undefined : { leagues }
}
