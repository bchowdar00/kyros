import type { WorkflowState } from '@mastra/core/workflows'
import { describe, expect, it, vi } from 'vitest'
import { buildStatus } from '@/mastra/strategy-builds'

const GENERIC_ERROR = 'The strategy writer hit an error. Try again in a moment.'

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
    series: 'btc-5m',
    legs: ['hunting', 'done'],
    subscriptions: ['btc-5m.current.price.v1'],
    instruments: { market: 'btc-5m.current' },
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
        series: 'btc-5m',
        legs: ['hunting', 'done'],
        subscriptions: ['btc-5m.current.price.v1'],
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

  it('keeps the plain message of a WriterUnavailableError', () => {
    const status = buildStatus({
      ...base,
      status: 'failed',
      error: {
        name: 'WriterUnavailableError',
        message: 'The strategy writer declined to answer this brief.',
      } as never,
    })
    expect(status).toMatchObject({
      status: 'error',
      error: 'The strategy writer declined to answer this brief.',
    })
  })

  it('sanitizes any other error and logs the original server-side', () => {
    const logger = { error: vi.fn() }
    const raw =
      'Error: 400 {"type":"error"} (request id: req_011ABCdef) headers: {"x-api-key":"sk-ant-secret"}'
    const status = buildStatus(
      { ...base, status: 'failed', error: { name: 'Error', message: raw } as never },
      logger
    )
    expect(status.status).toBe('error')
    expect(status.error).toBe(GENERIC_ERROR)
    // The raw SDK text never reaches the user-facing status.
    const publicStatus = JSON.stringify(status)
    expect(publicStatus).not.toContain('x-api-key')
    expect(publicStatus).not.toContain('req_011ABCdef')
    expect(publicStatus).not.toContain('sk-ant-secret')
    // The original is still logged for debugging.
    expect(logger.error).toHaveBeenCalledTimes(1)
  })

  it('maps a crashed run with no error object to the generic line', () => {
    const status = buildStatus({ ...base, status: 'failed' })
    expect(status).toMatchObject({ status: 'error', error: GENERIC_ERROR })
  })
})
