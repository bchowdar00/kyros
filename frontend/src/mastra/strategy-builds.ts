import type { Mastra } from '@mastra/core/mastra'
import type { WorkflowState, WorkflowStateStepResult } from '@mastra/core/workflows'
import * as z from 'zod/mini'
import {
  BUILD_STEP_IDS,
  BuildResultSchema,
  type BuildResult,
  type BuildStatus,
  type BuildStep,
} from '../features/market-detail/strategy-build'
import type { StrategyBrief } from './capabilities/strategy/brief'
import { COMPILE_STRATEGY_WORKFLOW_ID } from './workflows/compile-strategy'

/**
 * Strategy builds: one workflow run per click of Compile. A build belongs to the browser
 * session that started it, so a run id alone never reveals another user's build.
 */

function workflow(mastra: Mastra) {
  return mastra.getWorkflowById(COMPILE_STRATEGY_WORKFLOW_ID)
}

export async function startBuild(mastra: Mastra, resourceId: string, brief: StrategyBrief) {
  const run = await workflow(mastra).createRun({ resourceId })
  const { runId } = await run.startAsync({ inputData: { brief } })
  return runId
}

export async function readBuild(
  mastra: Mastra,
  resourceId: string,
  runId: string
): Promise<BuildStatus | null> {
  const state = await workflow(mastra).getWorkflowRunById(runId)
  if (!state || state.resourceId !== resourceId) return null
  return buildStatus(state)
}

function lastResult(result: WorkflowStateStepResult | undefined) {
  return Array.isArray(result) ? result.at(-1) : result
}

function stepStatus(state: WorkflowState, id: BuildStep['id']): BuildStep {
  const result = lastResult(state.steps?.[id])
  const attempts =
    id === 'compile' && typeof result?.output?.['attempts'] === 'number'
      ? { attempts: result.output['attempts'] as number }
      : {}
  switch (result?.status) {
    case 'success':
      return { id, status: 'done', ...attempts }
    case 'failed':
      return { id, status: 'failed', ...attempts }
    case 'running':
    case 'waiting':
    case 'suspended':
      return { id, status: 'running' }
    default:
      return { id, status: 'pending' }
  }
}

/** The compile step's output shape, as far as the card needs it. */
const CompiledSchema = z.object({
  status: z.enum(['compiled', 'failed']),
  ...BuildResultSchema.shape,
})

export function buildStatus(state: WorkflowState): BuildStatus {
  const steps = BUILD_STEP_IDS.map(id => stepStatus(state, id))
  const base = { id: state.runId, steps, result: null, error: null }
  switch (state.status) {
    case 'success': {
      const parsed = z.safeParse(CompiledSchema, state.result)
      if (!parsed.success)
        return { ...base, status: 'error', error: 'The build finished in an unexpected shape.' }
      const { status, ...result } = parsed.data
      const trimmed: BuildResult = { ...result, summary: result.summary }
      return { ...base, status, result: trimmed }
    }
    case 'failed':
    case 'tripwire':
    case 'canceled':
    case 'bailed':
      return { ...base, status: 'error', error: state.error?.message ?? 'The build stopped early.' }
    default:
      return { ...base, status: 'running' }
  }
}
