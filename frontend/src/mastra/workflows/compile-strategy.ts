import type { Agent } from '@mastra/core/agent'
import { createStep, createWorkflow } from '@mastra/core/workflows'
import { jsonSchema } from 'ai'
import * as z from 'zod/mini'
import { env } from '../../env'
import { STRATEGY_WRITER_AGENT_ID } from '../agents/strategy-writer'
import { AuthoringStrategySchema } from '../capabilities/strategy/authoring-schema'
import { briefSides, StrategyBriefSchema } from '../capabilities/strategy/brief'
import { CompileSummarySchema, compileStrategy } from '../capabilities/strategy/compile-client'
import { EnvelopeSchema, deriveEnvelope } from '../capabilities/strategy/envelope'
import { StrategyPlanSchema } from '../capabilities/strategy/plan'
import { checkPlan } from '../capabilities/strategy/plan-check'
import {
  planPrompt,
  repairPrompt,
  reviewPrompt,
  scoutPrompt,
  writePrompt,
} from '../capabilities/strategy/prompts'
import {
  createVenueRegistry,
  renderSignalDefinitions,
  selectSignals,
  signalIndex,
} from '../capabilities/strategy/registry'
import { compileWithRepairs } from '../capabilities/strategy/repair-loop'
import { checkDocumentAgainstPlan } from '../capabilities/strategy/review-check'

/** The id the build routes start and poll the workflow by. */
export const COMPILE_STRATEGY_WORKFLOW_ID = 'compile-strategy'

const BuildInputSchema = z.object({ brief: StrategyBriefSchema })

const ScoutOutputSchema = z.object({
  signalIds: z.array(z.string()),
  definitions: z.string(),
  indexed: z.int(),
})

const PlanOutputSchema = z.object({
  plan: StrategyPlanSchema,
  definitions: z.string(),
  /** How many plans the validator saw before accepting one. */
  planAttempts: z.int(),
})

const WriteOutputSchema = z.object({ authored: AuthoringStrategySchema, plan: StrategyPlanSchema })

/** Plans the validator refuses beyond this are handed on with their problems. */
const MAX_PLAN_ATTEMPTS = 3

const CompileOutputSchema = z.object({
  status: z.enum(['compiled', 'failed']),
  attempts: z.int(),
  authored: AuthoringStrategySchema,
  plan: StrategyPlanSchema,
  strategy: z.record(z.string(), z.unknown()),
  /** What the document was compiled against; null when the brief's money did not derive. */
  envelope: z.nullable(EnvelopeSchema),
  summary: z.nullable(CompileSummarySchema),
  warnings: z.array(z.string()),
  problems: z.array(z.string()),
  /** Plan mismatches still standing after the repair budget; empty when the shape is right. */
  mismatches: z.array(z.string()),
})

export const BuildResultSchema = z.object({
  ...CompileOutputSchema.shape,
  readback: z.string(),
  assumptions: z.array(z.string()),
  mismatches: z.array(z.string()),
})

function exchangeUrl() {
  const url = env.EXCHANGE_API_URL ?? env.NEXT_PUBLIC_API_URL
  if (!url) throw new Error('EXCHANGE_API_URL is not configured')
  return url
}

/**
 * One structured-output call. zod/mini does not carry a JSON schema of its own, so the
 * model is handed the generated one and the answer is validated with zod afterwards.
 */
async function generateObject<Schema extends z.ZodMiniType>(
  writer: Agent,
  prompt: string,
  schema: Schema
): Promise<z.infer<Schema>> {
  const result = await writer.generate(prompt, {
    structuredOutput: { schema: jsonSchema<z.infer<Schema>>(z.toJSONSchema(schema)) },
  })
  const parsed = z.safeParse(schema, result.object)
  if (!parsed.success) throw new Error('The writer did not answer in the requested shape.')
  return parsed.data
}

const scout = createStep({
  id: 'scout',
  inputSchema: BuildInputSchema,
  outputSchema: ScoutOutputSchema,
  execute: async ({ inputData, mastra }) => {
    const registry = createVenueRegistry(exchangeUrl())
    const catalogue = await registry.catalogue(inputData.brief.series)
    if (!catalogue.ok) throw new Error('The signal registry could not be read.')
    const index = signalIndex(catalogue.data)
    const writer = mastra.getAgentById(STRATEGY_WRITER_AGENT_ID)
    const choice = await generateObject(
      writer,
      scoutPrompt(inputData.brief, index),
      z.object({ signal_ids: z.array(z.string()) })
    )
    const book = `${inputData.brief.series}.current.price.v1`
    const wanted = [...new Set([...inputData.brief.signal_ids, ...choice.signal_ids, book])]
    const { chosen } = selectSignals(catalogue.data, wanted)
    return {
      signalIds: chosen.map(signal => signal.signal_id),
      definitions: renderSignalDefinitions(chosen),
      indexed: index.length,
    }
  },
})

/**
 * The structural plan: legs, wakes, edge policies, actions, memory and fates, checked
 * against the brief before any document exists. A refused plan goes back to the
 * writer with the reasons; the last attempt is used regardless so the build can
 * still report what went wrong in the user's terms.
 */
const plan = createStep({
  id: 'plan',
  inputSchema: ScoutOutputSchema,
  outputSchema: PlanOutputSchema,
  execute: async ({ inputData, getInitData, mastra }) => {
    const { brief } = z.parse(BuildInputSchema, getInitData())
    const writer = mastra.getAgentById(STRATEGY_WRITER_AGENT_ID)
    let problems: string[] = []
    let attempts = 0
    for (;;) {
      attempts += 1
      const proposed = await generateObject(
        writer,
        planPrompt(brief, inputData.definitions, problems),
        StrategyPlanSchema
      )
      problems = checkPlan(brief, proposed)
      if (problems.length === 0 || attempts >= MAX_PLAN_ATTEMPTS)
        return { plan: proposed, definitions: inputData.definitions, planAttempts: attempts }
    }
  },
})

const write = createStep({
  id: 'write',
  inputSchema: PlanOutputSchema,
  outputSchema: WriteOutputSchema,
  execute: async ({ inputData, getInitData, mastra }) => {
    const { brief } = z.parse(BuildInputSchema, getInitData())
    const writer = mastra.getAgentById(STRATEGY_WRITER_AGENT_ID)
    const authored = await generateObject(
      writer,
      writePrompt(brief, inputData.plan, inputData.definitions),
      AuthoringStrategySchema
    )
    return { authored, plan: inputData.plan }
  },
})

const compile = createStep({
  id: 'compile',
  inputSchema: WriteOutputSchema,
  outputSchema: CompileOutputSchema,
  execute: async ({ inputData, getInitData, mastra }) => {
    const { brief } = z.parse(BuildInputSchema, getInitData())
    // The envelope is baked into the program, so it is derived from the brief's money
    // slots here; a brief whose money does not work is a compile failure in the user's
    // words, not the compiler's.
    const derived = deriveEnvelope(brief)
    if (!derived.ok)
      return {
        status: 'failed' as const,
        attempts: 0,
        authored: inputData.authored,
        plan: inputData.plan,
        strategy: {},
        envelope: null,
        summary: null,
        warnings: [],
        problems: [derived.problem],
        mismatches: [],
      }
    const writer = mastra.getAgentById(STRATEGY_WRITER_AGENT_ID)
    const apiUrl = exchangeUrl()
    const result = await compileWithRepairs({
      authored: inputData.authored,
      compile: strategy => compileStrategy(apiUrl, strategy, derived.envelope),
      repair: (authored, problems) =>
        generateObject(writer, repairPrompt(authored, problems), AuthoringStrategySchema),
      // A document that compiles with the wrong shape is repaired like a diagnostic.
      check: (authored, summary) =>
        checkDocumentAgainstPlan(inputData.plan, authored, summary, briefSides(brief)),
    })
    return result.status === 'compiled'
      ? {
          ...result,
          plan: inputData.plan,
          envelope: derived.envelope,
          summary: result.summary,
          problems: [],
        }
      : {
          ...result,
          plan: inputData.plan,
          envelope: derived.envelope,
          summary: null,
          warnings: [],
          mismatches: [],
        }
  },
})

const review = createStep({
  id: 'review',
  inputSchema: CompileOutputSchema,
  outputSchema: BuildResultSchema,
  execute: async ({ inputData, getInitData, mastra }) => {
    if (inputData.status === 'failed' || inputData.summary === null)
      return { ...inputData, readback: '', assumptions: [], mismatches: [] }
    // The shape was checked mechanically; prose is written only for a document whose
    // legs, wakes and orders match the plan. Otherwise the mismatches are the review.
    if (inputData.mismatches.length > 0) return { ...inputData, readback: '', assumptions: [] }
    const { brief } = z.parse(BuildInputSchema, getInitData())
    const writer = mastra.getAgentById(STRATEGY_WRITER_AGENT_ID)
    const derived = deriveEnvelope(brief)
    const verdict = await generateObject(
      writer,
      reviewPrompt(
        brief,
        inputData.plan,
        inputData.authored,
        inputData.summary,
        derived.ok ? derived.money : null
      ),
      z.object({
        readback: z.string(),
        assumptions: z.array(z.string()),
        mismatches: z.array(z.string()),
      })
    )
    return { ...inputData, ...verdict }
  },
})

export const compileStrategyWorkflow = createWorkflow({
  id: COMPILE_STRATEGY_WORKFLOW_ID,
  inputSchema: BuildInputSchema,
  outputSchema: BuildResultSchema,
})
  .then(scout)
  .then(plan)
  .then(write)
  .then(compile)
  .then(review)
  .commit()
