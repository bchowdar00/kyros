/**
 * The two sides of a market, as the series labels them. Every exchange book is one
 * canonical book in the quoted side: a BTC round quotes `up` and `down` is its
 * complement; an NBA round quotes `home` and `away` is its complement. The authoring
 * machinery reasons in the quoted frame (`take_up` is "take the quoted side", a
 * positive position is the quoted side) and writes the series' own labels into the
 * document, because the compiler checks `outcome` against the registry verbatim.
 */
export interface Sides {
  /** The side the book quotes; a positive position holds it. */
  quoted: string
  /** Its complement; a negative position holds it. */
  other: string
}

export const BTC_SIDES: Sides = { quoted: 'up', other: 'down' }

/** The sides from a series' outcome list (`GET /api/series`), positive side first. */
export function sidesOf(outcomes: readonly string[] | undefined): Sides {
  const [quoted, other] = outcomes ?? []
  return quoted !== undefined && other !== undefined ? { quoted, other } : BTC_SIDES
}

/** The label of a side in the quoted frame. */
export function labelOf(sides: Sides, side: 'up' | 'down') {
  return side === 'up' ? sides.quoted : sides.other
}

/**
 * A brief's side in the quoted frame: the series' own label or the frame's `up`/`down`
 * both read; anything else is `either`, and a label that names neither side is
 * reported so the plan check can say so.
 */
export function normalizeOutcome(
  outcome: string,
  sides: Sides
): 'up' | 'down' | 'either' | undefined {
  const label = outcome.trim().toLowerCase()
  if (label === 'either' || label === 'whichever') return 'either'
  if (label === sides.quoted || label === 'up') return 'up'
  if (label === sides.other || label === 'down') return 'down'
  return undefined
}

/** `Home` from `home`. */
export function capitalize(label: string) {
  return label.charAt(0).toUpperCase() + label.slice(1)
}
