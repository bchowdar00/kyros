import * as z from 'zod/mini'
import { EnvelopeSchema } from '@/mastra/capabilities/strategy/envelope'

/** The wire shape of a strategy build, shared by the build routes and the build card. */

export const BUILD_STEP_IDS = ['scout', 'plan', 'write', 'compile', 'review'] as const
export type BuildStepId = (typeof BUILD_STEP_IDS)[number]

/** What each step is doing, in the user's words. */
export const BUILD_STEP_LABELS: Record<BuildStepId, { active: string; done: string }> = {
  scout: { active: 'Mapping signals…', done: 'Mapped signals' },
  plan: { active: 'Planning the states…', done: 'Planned the states' },
  write: { active: 'Writing the strategy…', done: 'Wrote the strategy' },
  compile: { active: 'Compiling…', done: 'Compiled' },
  review: { active: 'Reviewing against your brief…', done: 'Reviewed' },
}

export const BuildStepSchema = z.object({
  id: z.enum(BUILD_STEP_IDS),
  status: z.enum(['pending', 'running', 'done', 'failed']),
  /** Compile only: how many documents the compiler saw. */
  attempts: z.optional(z.int()),
})

export const BuildResultSchema = z.object({
  readback: z.string(),
  assumptions: z.array(z.string()),
  mismatches: z.array(z.string()),
  warnings: z.array(z.string()),
  problems: z.array(z.string()),
  attempts: z.int(),
  /** The compiled IR document, what a deploy sends back to the exchange. */
  strategy: z.record(z.string(), z.unknown()),
  /** The envelope it was compiled against; null when the brief's money did not derive. */
  envelope: z.nullable(EnvelopeSchema),
  summary: z.nullable(
    z.object({
      strategy_id: z.string(),
      name: z.string(),
      description: z.string(),
      series: z.string(),
      legs: z.array(z.string()),
      subscriptions: z.array(z.string()),
    })
  ),
})

export const BuildStatusSchema = z.object({
  id: z.string(),
  status: z.enum(['running', 'compiled', 'failed', 'error']),
  steps: z.array(BuildStepSchema),
  result: z.nullable(BuildResultSchema),
  error: z.nullable(z.string()),
})

export const BuildStartedSchema = z.object({ id: z.string() })

export type BuildStep = z.infer<typeof BuildStepSchema>
export type BuildResult = z.infer<typeof BuildResultSchema>
export type BuildStatus = z.infer<typeof BuildStatusSchema>
