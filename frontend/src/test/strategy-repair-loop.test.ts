import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { compileStrategy } from '@/mastra/capabilities/strategy/compile-client'
import type { Envelope } from '@/mastra/capabilities/strategy/envelope'
import { compileWithRepairs } from '@/mastra/capabilities/strategy/repair-loop'
import { DERIVATIONS } from '@/mastra/capabilities/strategy/derivations'
import { checkDocumentAgainstPlan } from '@/mastra/capabilities/strategy/review-check'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const [derivation] = DERIVATIONS
if (!derivation) throw new Error('derivations exist')
const authored = derivation.document
const ENVELOPE: Envelope = {
  allocation: '100',
  loss_budget: '100',
  round_budget: '10',
  max_order_size: '2',
  max_position_per_market: '10',
  max_open_orders: '4',
}

const summary = {
  strategy_id: 'Strategy_abc',
  name: authored.name,
  description: authored.description,
  series: 'btc-2m',
  subscriptions: ['btc-2m.current.price.v1'],
  instruments: { market: 'btc-2m.current' },
  params: [],
  legs: ['hunting', 'holding', 'done'],
  warnings: [
    {
      severity: 'warning',
      path: 'legs[0].body[0]',
      detail: { rule: { code: 'lint-coverage', message: 'a guard is never true', help: 'x' } },
    },
  ],
}
const refusal = {
  error: 'compile_failed',
  message: 'strategy did not compile: 1 error(s)',
  detail: {
    diagnostics: [],
    rendered: [
      'error[not-clamp-wrapped] at legs[0].body[0].do[0].place.size: a size must be clamp-wrapped',
    ],
  },
}

describe('compileStrategy', () => {
  it('returns the summary and rendered warnings on success', async () => {
    server.use(
      http.post('http://exchange.test/api/strategy/compile', () => HttpResponse.json(summary))
    )
    const outcome = await compileStrategy('http://exchange.test', {}, ENVELOPE)
    expect(outcome).toMatchObject({
      ok: true,
      warnings: ['warning[lint-coverage] at legs[0].body[0]: a guard is never true'],
    })
  })

  it('returns the compiler diagnostics on a 422', async () => {
    server.use(
      http.post('http://exchange.test/api/strategy/compile', () =>
        HttpResponse.json(refusal, { status: 422 })
      )
    )
    const outcome = await compileStrategy('http://exchange.test', {}, ENVELOPE)
    expect(outcome).toEqual({
      ok: false,
      kind: 'diagnostics',
      rendered: refusal.detail.rendered,
      message: refusal.message,
    })
  })

  it('reports a plain-text deserialization rejection', async () => {
    server.use(
      http.post(
        'http://exchange.test/api/strategy/compile',
        () =>
          new HttpResponse('Failed to deserialize the JSON body: missing field `meta`', {
            status: 422,
          })
      )
    )
    const outcome = await compileStrategy('http://exchange.test', {}, ENVELOPE)
    expect(outcome).toMatchObject({ ok: false, kind: 'rejected', message: /missing field/ })
  })
})

describe('compileWithRepairs', () => {
  it('compiles on the first attempt without asking for a repair', async () => {
    const repair = vi.fn()
    const result = await compileWithRepairs({
      authored,
      compile: async () => ({ ok: true, summary, warnings: [] }),
      repair,
    })
    expect(result).toMatchObject({ status: 'compiled', attempts: 1 })
    expect(repair).not.toHaveBeenCalled()
  })

  it('feeds the diagnostics to the repair and accepts the corrected document', async () => {
    let calls = 0
    const repair = vi.fn(async (doc: typeof authored) => ({ ...doc, name: 'Repaired' }))
    const result = await compileWithRepairs({
      authored,
      compile: async strategy => {
        calls += 1
        const meta = strategy['meta'] as { name: string }
        return meta.name === 'Repaired'
          ? { ok: true, summary, warnings: [] }
          : { ok: false, kind: 'diagnostics', rendered: refusal.detail.rendered, message: '' }
      },
      repair,
    })
    expect(repair).toHaveBeenCalledWith(authored, refusal.detail.rendered)
    expect(result).toMatchObject({ status: 'compiled', attempts: 2 })
    expect(calls).toBe(2)
  })

  it('gives up after the repair budget with the last problems', async () => {
    const repair = vi.fn(async (doc: typeof authored) => doc)
    const result = await compileWithRepairs({
      authored,
      compile: async () => ({ ok: false, kind: 'rejected', message: 'nope' }),
      repair,
      maxRepairs: 2,
    })
    expect(result).toMatchObject({ status: 'failed', attempts: 3, problems: ['nope'] })
    expect(repair).toHaveBeenCalledTimes(2)
  })

  it('repairs a document that compiles with a structural warning', async () => {
    const machineGun = {
      ...summary,
      warnings: [
        {
          severity: 'warning',
          path: 'legs[0].body[0]',
          detail: {
            rule: { code: 'lint-machine-gun', message: 'nothing stops it re-firing', help: 'x' },
          },
        },
      ],
    }
    let calls = 0
    const repair = vi.fn(async (doc: typeof authored) => ({ ...doc, name: 'Paced' }))
    const result = await compileWithRepairs({
      authored,
      compile: async strategy => {
        calls += 1
        const meta = strategy['meta'] as { name: string }
        return {
          ok: true,
          summary,
          warnings:
            meta.name === 'Paced'
              ? []
              : ['warning[lint-machine-gun] at legs[0].body[0]: nothing stops it re-firing'],
        }
      },
      repair,
    })
    expect(machineGun.warnings).toHaveLength(1)
    expect(repair).toHaveBeenCalledWith(authored, [expect.stringContaining('lint-machine-gun')])
    expect(result).toMatchObject({ status: 'compiled', attempts: 2, mismatches: [] })
    expect(calls).toBe(2)
  })

  it('repairs a document that compiles but does not match the plan', async () => {
    const twoState = {
      ...authored,
      legs: authored.legs.filter(l => l.name !== 'holding'),
      blocks: authored.blocks.filter(b => b.leg !== 'holding'),
    }
    const repair = vi.fn(async (_doc: typeof authored, _problems: string[]) => authored)
    const result = await compileWithRepairs({
      authored: twoState,
      compile: async () => ({ ok: true, summary, warnings: [] }),
      repair,
      check: doc => checkDocumentAgainstPlan(derivation.plan, doc),
    })
    expect(repair).toHaveBeenCalledTimes(1)
    expect(repair.mock.calls[0]?.[1]).toContainEqual(expect.stringContaining('the plan has legs'))
    expect(result).toMatchObject({ status: 'compiled', attempts: 2, mismatches: [] })
  })

  it('ships a compiling document with its mismatches when the repair budget runs out', async () => {
    const repair = vi.fn(async (doc: typeof authored) => doc)
    const result = await compileWithRepairs({
      authored,
      compile: async () => ({ ok: true, summary, warnings: [] }),
      repair,
      check: () => ['leg `holding` arms nothing but the plan wakes it on book_tick'],
      maxRepairs: 1,
    })
    expect(repair).toHaveBeenCalledTimes(1)
    expect(result).toMatchObject({
      status: 'compiled',
      attempts: 2,
      mismatches: [expect.stringContaining('arms nothing')],
    })
  })

  it('treats a projection warning as a problem before calling the compiler', async () => {
    const compile = vi.fn()
    const [block] = authored.blocks
    if (!block) throw new Error('derivation has blocks')
    const broken = { ...authored, blocks: [{ ...block, leg: 'nowhere' }] }
    const result = await compileWithRepairs({
      authored: broken,
      compile,
      repair: async () => broken,
      maxRepairs: 1,
    })
    expect(compile).not.toHaveBeenCalled()
    expect(result).toMatchObject({
      status: 'failed',
      problems: [expect.stringContaining('unknown leg')],
    })
  })
})
