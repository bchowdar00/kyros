import { describe, expect, it } from 'vitest'
import * as z from 'zod/mini'

import { EventSchema, MarketSchema } from './catalog'
import { OutcomeQuoteSchema } from './pricing'
import { EntityIdSchema, SlugSchema } from './primitives'

const moneyline = {
  id: 300_001,
  event_id: 200_001,
  slug: 'bos-lal-moneyline',
  question: 'Who wins?',
  rules: null,
  type: 'moneyline',
  period: null,
  stat: null,
  line: null,
  subject: null,
  is_main_line: true,
  image: null,
  settlement_sources: ['https://www.nba.com/'],
  outcomes: [
    { id: 400_001, market_id: 300_001, label: 'Celtics', sort: 0, image: null },
    { id: 400_002, market_id: 300_001, label: 'Lakers', sort: 1, image: null },
  ],
} satisfies z.input<typeof MarketSchema>

const event = {
  id: 200_001,
  slug: 'lakers-at-celtics-2026-11-04',
  title: 'Lakers @ Celtics',
  category: 'sports/basketball/nba',
  series_id: 100_001,
  source: 'sports',
  image: null,
  opens_at: '2026-11-03T00:30:00Z',
  occurs_at: '2026-11-04T00:30:00Z',
  closes_at: '2026-11-04T03:30:00Z',
  stage: 'regular',
  round: null,
  markets: [moneyline],
} satisfies z.input<typeof EventSchema>

describe('catalog contracts', () => {
  it('parses a sports event with an embedded moneyline', () => {
    const parsed = z.parse(EventSchema, event)
    expect(parsed.markets[0]?.outcomes.map(outcome => outcome.label)).toEqual(['Celtics', 'Lakers'])
  })

  it('accepts a total with a decimal line and a player subject', () => {
    const total = z.parse(MarketSchema, {
      ...moneyline,
      type: 'total',
      stat: 'points',
      line: '27.5',
      subject: { id: 500_001, kind: 'player', name: 'LeBron James' },
      outcomes: [
        { id: 400_003, market_id: 300_001, label: 'Over', sort: 0, image: null },
        { id: 400_004, market_id: 300_001, label: 'Under', sort: 1, image: null },
      ],
    })
    expect(total.line).toBe('27.5')
  })

  it('rejects a float line, a five-digit id, and an uppercase slug', () => {
    expect(z.safeParse(MarketSchema, { ...moneyline, line: 27.5 }).success).toBe(false)
    expect(z.safeParse(EntityIdSchema, 99_999).success).toBe(false)
    expect(z.safeParse(SlugSchema, 'Lakers').success).toBe(false)
  })
})

describe('pricing contracts', () => {
  it('turns wire integers into branded money values', () => {
    const quote = z.parse(OutcomeQuoteSchema, {
      outcome_id: 400_001,
      price_cents: 31,
      probability_bps: 3_100,
      as_of: '2026-11-04T01:15:00Z',
    })
    expect(quote.price_cents).toBe(31)
    expect(quote.probability_bps).toBe(3_100)
  })

  it('rejects float cents and out-of-range probabilities', () => {
    const base = { outcome_id: 400_001, as_of: '2026-11-04T01:15:00Z' }
    expect(
      z.safeParse(OutcomeQuoteSchema, { ...base, price_cents: 31.5, probability_bps: 3_150 })
        .success
    ).toBe(false)
    expect(
      z.safeParse(OutcomeQuoteSchema, { ...base, price_cents: 31, probability_bps: 10_001 }).success
    ).toBe(false)
  })
})
