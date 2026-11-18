import { createTool } from '@mastra/core/tools'
import * as z from 'zod/mini'
import { STANDALONE_CHAT } from '../../market-context'
import { readChatContext } from '../../request-context'
import { renderBrief, StrategyBriefSchema } from './brief'
import { deriveEnvelope, describeMoney } from './envelope'
import { AskQuestionInputSchema, resolveQuestion } from './question-catalogue'
import { createVenueRegistry, signalIndex } from './registry'
import { normalizeOutcome, sidesOf } from './sides'

export function createStrategyTools(apiUrl: string) {
  const registry = createVenueRegistry(apiUrl)

  const listMarketSignals = createTool({
    id: 'list-market-signals',
    description:
      'List the signals a strategy on this chat’s market can read: id, name, typed fields and a one-line summary. Only signals currently publishing are listed. Call this before naming any signal in a brief.',
    inputSchema: z.object({}),
    execute: async (_input, context) => {
      const { marketSlug } = readChatContext(context)
      if (!marketSlug || marketSlug === STANDALONE_CHAT)
        return { status: 'unavailable', reason: 'Open a market page to build a strategy for it.' }
      const catalogue = await registry.catalogue(marketSlug, context?.abortSignal)
      if (!catalogue.ok)
        return { status: 'unavailable', reason: 'The signal registry could not be read.' }
      const series = await registry.series(marketSlug, context?.abortSignal)
      return {
        status: 'ok',
        series: marketSlug,
        roundSeconds: series?.duration_secs ?? null,
        signals: signalIndex(catalogue.data),
      }
    },
  })

  const proposeCompilation = createTool({
    id: 'propose-compilation',
    description:
      'Show the user the agreed strategy brief with a Compile button. Call it once the brief is complete and the user has asked to build; it has no side effects and nothing compiles until the user confirms.',
    inputSchema: StrategyBriefSchema,
    execute: async (brief, context) => {
      const { marketSlug } = readChatContext(context)
      const series = marketSlug && marketSlug !== STANDALONE_CHAT ? marketSlug : brief.series
      // The market's side labels come from the exchange, never from the model.
      const info = await registry.series(series, context?.abortSignal)
      const sides = info?.outcomes ?? brief.sides
      const proposed = { ...brief, series, ...(sides === undefined ? {} : { sides }) }
      const side = normalizeOutcome(proposed.outcome, sidesOf(sides))
      if (side === undefined) {
        const { quoted, other } = sidesOf(sides)
        return {
          status: 'side_problem',
          problem: `\`${brief.outcome}\` is not a side of this market; it is ${quoted}, ${other} or either.`,
        }
      }
      // The money must derive into an envelope before the card is worth showing.
      const derived = deriveEnvelope(proposed)
      if (!derived.ok) return { status: 'money_problem', problem: derived.problem }
      return {
        status: 'proposed',
        brief: proposed,
        summary: renderBrief(proposed),
        money: describeMoney(proposed),
      }
    },
  })

  const askQuestion = createTool({
    id: 'ask-question',
    description:
      'Ask the user one multiple-choice question about the brief when two readings of their words would trade differently. Pick a catalogue kind (side, recurrence, entry_style, exit, stop_time) to get its short options, or `custom` with your own 2 to 4 short readings. Call it at most once per turn, then end your turn: the answer arrives as the user’s next message.',
    inputSchema: AskQuestionInputSchema,
    execute: async (input, context) => {
      // The side question offers the market's own labels.
      const { marketSlug } = readChatContext(context)
      const info =
        input.kind === 'side' && marketSlug && marketSlug !== STANDALONE_CHAT
          ? await registry.series(marketSlug, context?.abortSignal)
          : undefined
      return resolveQuestion(input, sidesOf(info?.outcomes))
    },
  })

  return { listMarketSignals, proposeCompilation, askQuestion }
}
