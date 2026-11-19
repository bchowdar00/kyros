import { describe, expect, it } from 'vitest'
import type { AuthoringStrategy } from '@/mastra/capabilities/strategy/authoring-schema'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { DERIVATIONS } from '@/mastra/capabilities/strategy/derivations'
import { deriveEnvelope } from '@/mastra/capabilities/strategy/envelope'
import { fragmentHost } from '@/mastra/capabilities/strategy/fragment-host'
import { FRAGMENTS, restBid } from '@/mastra/capabilities/strategy/fragments'
import { projectToIr } from '@/mastra/capabilities/strategy/project-to-ir'

// The worked example: $100 in, $10 per round, $2.50 per order, stop at $100.
const brief: StrategyBrief = {
  series: 'btc-5m',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'g',
  entry: 'e',
  signal_ids: [],
  outcome: 'up',
  recurrence: 'once_per_round',
  interval: null,
  entry_style: 'market',
  exit: 'hold to settlement',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: [],
}
const derived = deriveEnvelope(brief)
const envelope = derived.ok ? derived.envelope : undefined

// Runs against a local exchange that serves the compile route when one is listening;
// otherwise the suite is skipped so `pnpm test` stays green without the Rust server.
// Start it with `cargo run` (or `PORT=8081 cargo run`) in server/.
// Set EXCHANGE_TEST_URL to compile against a specific exchange (worktrees run on other ports).
// oxlint-disable-next-line node/no-process-env -- a test-only override, never read by app code
const OVERRIDE = process.env['EXCHANGE_TEST_URL']
const CANDIDATE_URLS = ['http://localhost:8081', 'http://localhost:8080']

async function findExchange() {
  for (const url of OVERRIDE ? [OVERRIDE] : CANDIDATE_URLS) {
    try {
      const response = await fetch(`${url}/api/strategy/schema`, {
        signal: AbortSignal.timeout(500),
      })
      if (response.ok) return url
    } catch {
      // Not listening here; try the next candidate.
    }
  }
  return undefined
}

const EXCHANGE_URL = await findExchange()

interface Compiled {
  legs?: string[]
  warnings?: unknown[]
}

async function compile(document: AuthoringStrategy) {
  const { strategy, warnings } = projectToIr(document)
  expect(warnings).toEqual([])
  const response = await fetch(`${EXCHANGE_URL}/api/strategy/compile`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ strategy, envelope }),
  })
  const body: unknown = await response.json()
  expect(response.status, JSON.stringify(body, null, 2)).toBe(200)
  // The document names its own series.
  const market = document.instruments.find(i => i.nick === 'market')?.id ?? 'btc-5m.current'
  const series = market.replace(/\.current$/, '')
  expect(body).toMatchObject({ series, instruments: { market } })
  return body as Compiled
}

describe.skipIf(EXCHANGE_URL === undefined)('authoring material against the exchange', () => {
  it.each(DERIVATIONS.map(d => [d.id, d] as const))(
    'derivation %s compiles without warnings',
    async (_id, derivation) => {
      const compiled = await compile(derivation.document)
      expect(compiled.legs).toEqual(derivation.plan.legs.map(leg => leg.name))
      expect(compiled.warnings, JSON.stringify(compiled.warnings, null, 2)).toEqual([])
    }
  )

  const kinds = Object.keys(FRAGMENTS) as (keyof typeof FRAGMENTS)[]
  it.each(kinds)('fragment %s compiles inside its host without warnings', async kind => {
    // A cancel needs a tag some place declares; the host rests one first.
    const items =
      kind === 'cancel'
        ? [restBid('0.25', 'bid', null), ...FRAGMENTS.cancel.items]
        : FRAGMENTS[kind].items
    const compiled = await compile(fragmentHost('btc-5m', items))
    expect(compiled.warnings, JSON.stringify(compiled.warnings, null, 2)).toEqual([])
  })
})
