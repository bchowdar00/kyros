import * as z from 'zod/mini'

/**
 * Gemini's public list of prediction markets (`GET /v1/prediction-markets/events`), turned
 * into the cards the Markets page shows under the three series Kyros trades. Only the fields
 * read here are described; an entry that does not fit is skipped, never guessed at.
 */

const PriceSchema = z.optional(z.nullable(z.string()))

const ContractSchema = z.object({
  label: z.string(),
  prices: z.object({
    buy: z.optional(z.object({ no: PriceSchema, yes: PriceSchema })),
    lastTradePrice: PriceSchema,
  }),
  strike: z.optional(z.nullable(z.object({ value: PriceSchema }))),
})

const EventSchema = z.object({
  contracts: z.array(ContractSchema),
  expiryDate: z.string(),
  participants: z.optional(z.array(z.object({ abbreviation: z.string() }))),
  series: z.optional(z.string()),
  startTime: z.string(),
  subcategory: z.optional(z.object({ name: z.string() })),
  template: z.optional(z.string()),
  ticker: z.string(),
  title: z.string(),
})

type GeminiEvent = z.infer<typeof EventSchema>

/** One question market's contract: its label and the prices each side trades at now. */
const QuestionContractSchema = z.object({
  label: z.string(),
  marketState: z.optional(z.string()),
  prices: z.object({ buy: z.optional(z.object({ no: PriceSchema, yes: PriceSchema })) }),
  status: z.optional(z.string()),
})

/** Any Gemini event, read for the fields a question card needs. */
const QuestionEventSchema = z.object({
  category: z.string(),
  contracts: z.array(QuestionContractSchema),
  expiryDate: z.optional(z.string()),
  series: z.optional(z.string()),
  status: z.string(),
  template: z.optional(z.string()),
  ticker: z.string(),
  title: z.string(),
  type: z.string(),
  volume24h: z.optional(z.string()),
})

type QuestionContract = z.infer<typeof QuestionContractSchema>

/** One up/down series (an asset and a round length) at the round that is open now. */
export interface UpDownCard {
  asset: string
  /** When the open round closes, ISO 8601. */
  closesAt: string
  /** What Down costs to buy now, in cents; absent while the book has no price. */
  downCents?: number
  /** Gemini's series code, `BTC05M`. */
  id: string
  kind: 'updown'
  minutes: number
  /** The price the round is measured against; absent until Gemini publishes it. */
  strike?: number
  title: string
  /** The Kyros market that trades this series; absent when Kyros only shows it. */
  tradeSlug?: string
  upCents?: number
}

/** One game's winner market. */
export interface GameCard {
  /** Gemini's event ticker. */
  id: string
  kind: 'game'
  league: string
  outcomes: readonly { cents?: number; label: string }[]
  /** True once the scheduled start has passed. */
  started: boolean
  startsAt: string
  /** The teams' short codes, as Gemini lists them. */
  teams: readonly string[]
  title: string
  tradeSlug?: string
}

/** One active market that is neither an up/down round nor a sports game. */
export interface QuestionCard {
  /** Gemini's category name, as given, e.g. `Politics`. */
  category: string
  /** When the market closes, ISO 8601; absent when Gemini gives no expiry. */
  closesAt?: string
  /** The event's ticker. */
  id: string
  kind: 'question'
  /** How many open outcomes exist beyond the ones listed, priced or not; 0 when none do. */
  more: number
  /** At most three outcomes; a side with no price carries no cents. */
  outcomes: { cents?: number; label: string }[]
  title: string
}

const CENTS_PER_DOLLAR = 100
const LOWEST_CENTS = 1
const HIGHEST_CENTS = 99
/** Both sides near a dollar is an empty book quoting its ceiling, not a price for either. */
const EMPTY_BOOK_SUM_CENTS = 130
/** Outcomes listed on a question card; the rest are counted in `more`. */
const MAX_OUTCOMES = 3
/**
 * A game that started longer ago than this is over and only waiting to be settled. Gemini's
 * list gives no score and no final flag, so the clock is the only evidence: four hours covers
 * the longest of these sports.
 */
const GAME_WINDOW_MS = 4 * 60 * 60 * 1000

/** The series Kyros trades, by Gemini's series code. */
const kyrosSeries: Readonly<Record<string, string>> = {
  BTC05M: 'btc-5m',
  BTC15M: 'btc-15m',
}

/** Gemini's league names as a sports page says them. */
const leagueNames: Readonly<Record<string, string>> = {
  'College Basketball': 'NCAA Basketball',
  'College Football': 'NCAA Football',
  'Pro Basketball': 'NBA',
  'Pro Football': 'NFL',
  'Pro Hockey': 'NHL',
}

function readable<T>(entries: readonly unknown[], schema: z.ZodMiniType<T>): T[] {
  const events: T[] = []
  for (const entry of entries) {
    const parsed = z.safeParse(schema, entry)
    if (parsed.success) events.push(parsed.data)
  }
  return events
}

/** The one test that marks an event as an up/down round, shared by both readers. */
function isUpDownRound<T extends { series?: string | undefined; template?: string | undefined }>(
  event: T
): event is T & { series: string } {
  return event.template === 'crypto-up-down' && event.series !== undefined
}

/**
 * The cents each side of a contract's book trades at. Both sides near a dollar is an empty
 * book quoting its ceiling, not a price for either, so neither is given.
 */
function bookPrice(buy: QuestionContract['prices']['buy']): { no?: number; yes?: number } {
  const yes = cents(buy?.yes)
  const no = cents(buy?.no)
  if (yes !== undefined && no !== undefined && yes + no >= EMPTY_BOOK_SUM_CENTS) return {}
  return { ...(no === undefined ? {} : { no }), ...(yes === undefined ? {} : { yes }) }
}

/** By price, highest first, with unpriced outcomes last, ties broken by label. */
function byPriceThenLabel(
  a: { cents?: number; label: string },
  b: { cents?: number; label: string }
): number {
  return (b.cents ?? -1) - (a.cents ?? -1) || a.label.localeCompare(b.label)
}

/** A dollar price string as whole cents, when it is a price a side can trade at. */
function cents(price: string | null | undefined): number | undefined {
  if (price === undefined || price === null) return undefined
  const value = Math.round(Number(price) * CENTS_PER_DOLLAR)
  return Number.isFinite(value) && value >= LOWEST_CENTS && value <= HIGHEST_CENTS
    ? value
    : undefined
}

/**
 * One card per up/down series: the round open at `now`, or the next to open when none is.
 * In Gemini's order of series.
 */
export function toUpDownCards(entries: readonly unknown[], now: number): UpDownCard[] {
  const rounds = new Map<string, GeminiEvent>()
  for (const event of readable(entries, EventSchema)) {
    if (!isUpDownRound(event)) continue
    const { series } = event
    if (!(Date.parse(event.expiryDate) > now)) continue
    const kept = rounds.get(series)
    // Rounds do not overlap, so the earliest one still to close is the open or the next one.
    if (kept === undefined || Date.parse(event.startTime) < Date.parse(kept.startTime)) {
      rounds.set(series, event)
    }
  }

  const cards: UpDownCard[] = []
  for (const [series, event] of rounds) {
    const code = /^([A-Z]+)(\d+)M$/.exec(series)
    const [contract] = event.contracts
    if (code === null || contract === undefined) continue
    const [, asset = '', length = ''] = code
    const minutes = Number(length)
    const up = cents(contract.prices.buy?.yes)
    const down = cents(contract.prices.buy?.no)
    const priced = up !== undefined && down !== undefined && up + down < EMPTY_BOOK_SUM_CENTS
    const strike = Number(contract.strike?.value ?? Number.NaN)
    const tradeSlug = kyrosSeries[series]

    cards.push({
      asset,
      closesAt: event.expiryDate,
      ...(priced ? { downCents: down } : {}),
      id: series,
      kind: 'updown',
      minutes,
      ...(Number.isFinite(strike) ? { strike } : {}),
      title: `${asset} Up or Down ${minutes}m`,
      ...(tradeSlug === undefined ? {} : { tradeSlug }),
      ...(priced ? { upCents: up } : {}),
    })
  }
  return cards
}

/** Every game still to be played or in play, soonest first. */
export function toGameCards(entries: readonly unknown[], now: number): GameCard[] {
  const cards: GameCard[] = []
  for (const event of readable(entries, EventSchema)) {
    if (event.template !== 'sports-game') continue
    const startsAt = Date.parse(event.startTime)
    if (!(Date.parse(event.expiryDate) > now) || !(startsAt > now - GAME_WINDOW_MS)) continue
    const league = event.subcategory?.name ?? 'Sports'

    cards.push({
      id: event.ticker,
      kind: 'game',
      league: leagueNames[league] ?? league,
      outcomes: event.contracts.map(contract => {
        const price = cents(contract.prices.buy?.yes) ?? cents(contract.prices.lastTradePrice)
        return { ...(price === undefined ? {} : { cents: price }), label: contract.label }
      }),
      started: startsAt <= now,
      startsAt: event.startTime,
      teams: (event.participants ?? []).map(team => team.abbreviation),
      title: event.title,
    })
  }

  return cards.toSorted(
    (a, b) =>
      Date.parse(a.startsAt) - Date.parse(b.startsAt) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)
  )
}

/** Marks the one basketball game Kyros trades, matched by its two team codes. */
export function withKyrosGame(
  games: readonly GameCard[],
  kyros: { slug: string; teams: readonly string[] } | undefined
): GameCard[] {
  if (kyros === undefined || kyros.teams.length === 0) return [...games]
  const wanted = [...kyros.teams].toSorted().join('|')
  return games.map(game =>
    game.league === 'NBA' && [...game.teams].toSorted().join('|') === wanted
      ? { ...game, tradeSlug: kyros.slug }
      : game
  )
}

/** The outcomes of a binary event: Yes and No, at the single contract's buy prices. */
function binaryOutcomes(
  contract: QuestionContract | undefined
): { cents?: number; label: string }[] {
  if (contract === undefined) return []
  const { no, yes } = bookPrice(contract.prices.buy)
  return [
    { ...(yes === undefined ? {} : { cents: yes }), label: 'Yes' },
    { ...(no === undefined ? {} : { cents: no }), label: 'No' },
  ]
}

/**
 * One card per active question market: every event that is neither an up/down round nor a
 * sports game. Ordered by 24-hour volume, then by close, then by title; volume orders only
 * and is not shown. An entry that cannot be read is skipped, never guessed at.
 */
export function toQuestionCards(entries: readonly unknown[], now: number): QuestionCard[] {
  const ranked: { card: QuestionCard; closesMs: number; volume: number }[] = []
  for (const event of readable(entries, QuestionEventSchema)) {
    if (isUpDownRound(event) || event.category === 'Sports' || event.status !== 'active') continue
    const closesMs = event.expiryDate === undefined ? Number.NaN : Date.parse(event.expiryDate)
    if (Number.isFinite(closesMs) && closesMs <= now) continue

    const open = event.contracts.filter(
      contract => contract.status === 'active' && contract.marketState === 'open'
    )
    const outcomes =
      event.type === 'binary'
        ? binaryOutcomes(open[0])
        : open
            .map(contract => {
              const { yes } = bookPrice(contract.prices.buy)
              return { ...(yes === undefined ? {} : { cents: yes }), label: contract.label }
            })
            .toSorted(byPriceThenLabel)

    const volume = Number(event.volume24h)
    ranked.push({
      card: {
        category: event.category,
        // A close date that cannot be read is left off the card, not passed on to break it.
        ...(event.expiryDate === undefined || !Number.isFinite(closesMs)
          ? {}
          : { closesAt: event.expiryDate }),
        id: event.ticker,
        kind: 'question',
        more: Math.max(0, outcomes.length - MAX_OUTCOMES),
        outcomes: outcomes.slice(0, MAX_OUTCOMES),
        title: event.title,
      },
      closesMs,
      volume: Number.isFinite(volume) ? volume : 0,
    })
  }

  return ranked
    .toSorted(
      (a, b) =>
        b.volume - a.volume ||
        (a.closesMs || Number.POSITIVE_INFINITY) - (b.closesMs || Number.POSITIVE_INFINITY) ||
        a.card.title.localeCompare(b.card.title)
    )
    .map(entry => entry.card)
}
