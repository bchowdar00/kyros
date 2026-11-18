import type { WorkflowState } from '@mastra/core/workflows'
import { describe, expect, it } from 'vitest'
import { buildStatus } from '@/mastra/strategy-builds'

const base: WorkflowState = {
  runId: 'run-1',
  workflowName: 'compile-strategy',
  resourceId: 'alice',
  createdAt: new Date(0),
  updatedAt: new Date(0),
  status: 'running',
}

const result = {
  status: 'compiled',
  attempts: 2,
  authored: {},
  strategy: { meta: { name: 'Threshold Entry' } },
  envelope: {
    allocation: '100',
    loss_budget: '100',
    round_budget: '10',
    max_order_size: '2',
    max_position_per_market: '10',
    max_open_orders: '4',
  },
  summary: {
    strategy_id: 'Strategy_abc',
    name: 'Threshold Entry',
    description: 'Buys up above 60 cents.',
    series: 'btc-2m',
    legs: ['hunting', 'done'],
    subscriptions: ['btc-2m.current.price.v1'],
    instruments: { market: 'btc-2m.current' },
    params: [],
    warnings: [],
  },
  warnings: ['warning[lint-coverage] at legs[0]: never true'],
  problems: [],
  readback: 'It buys up when the price crosses 60 cents.',
  assumptions: ['Full size per order.'],
  mismatches: [],
}

describe('buildStatus', () => {
  it('reports a running build with per-step progress', () => {
    const status = buildStatus({
      ...base,
      steps: {
        scout: { status: 'success' },
        plan: { status: 'success' },
        write: { status: 'running' },
      },
    })
    expect(status).toEqual({
      id: 'run-1',
      status: 'running',
      steps: [
        { id: 'scout', status: 'done' },
        { id: 'plan', status: 'done' },
        { id: 'write', status: 'running' },
        { id: 'compile', status: 'pending' },
        { id: 'review', status: 'pending' },
      ],
      result: null,
      error: null,
    })
  })

  it('carries the readback, warnings and compile attempts of a finished build', () => {
    const status = buildStatus({
      ...base,
      status: 'success',
      result,
      steps: {
        scout: { status: 'success' },
        plan: { status: 'success' },
        write: { status: 'success' },
        compile: { status: 'success', output: { attempts: 2 } },
        review: { status: 'success' },
      },
    })
    expect(status.status).toBe('compiled')
    expect(status.steps[3]).toEqual({ id: 'compile', status: 'done', attempts: 2 })
    expect(status.result).toEqual({
      readback: result.readback,
      assumptions: result.assumptions,
      mismatches: [],
      warnings: result.warnings,
      problems: [],
      attempts: 2,
      strategy: result.strategy,
      envelope: result.envelope,
      summary: {
        strategy_id: 'Strategy_abc',
        name: 'Threshold Entry',
        description: 'Buys up above 60 cents.',
        series: 'btc-2m',
        legs: ['hunting', 'done'],
        subscriptions: ['btc-2m.current.price.v1'],
      },
    })
  })

  it('reports a document the compiler kept refusing as failed with its problems', () => {
    const status = buildStatus({
      ...base,
      status: 'success',
      result: {
        ...result,
        status: 'failed',
        summary: null,
        problems: ['error[x] at y: z'],
        readback: '',
      },
    })
    expect(status.status).toBe('failed')
    expect(status.result?.problems).toEqual(['error[x] at y: z'])
    expect(status.result?.summary).toBeNull()
  })

  it('reports a crashed run as an error with the message', () => {
    const status = buildStatus({ ...base, status: 'failed', error: { message: 'boom' } as never })
    expect(status).toMatchObject({ status: 'error', error: 'boom' })
  })
})
