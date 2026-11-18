import { Agent } from '@mastra/core/agent'
import { env } from '../../env'
import { RULEBOOK } from '../capabilities/strategy/rulebook'
import { writerModelRouter } from '../writer-models'

/** The id this agent registers under. */
const STRATEGY_WRITER_AGENT_ID = 'strategy-writer'

/**
 * The writer behind the compile workflow: no tools, no memory, no persona. Every call
 * carries its whole context in the prompt and answers with a structured object.
 */
export const strategyWriter = new Agent({
  id: STRATEGY_WRITER_AGENT_ID,
  name: 'Strategy writer',
  description: 'Turns an agreed strategy brief into a compilable strategy document.',
  instructions: RULEBOOK,
  // The compile workflow drives the writer through the official Anthropic SDK (see
  // `mastra/writer-model.ts`) to control the exact request; this model is the same one.
  model: writerModelRouter(env.WRITER_MODEL),
})
