import { Agent } from '@mastra/core/agent'
import { RULEBOOK } from '../capabilities/strategy/rulebook'

/** The id the compile workflow looks the writer up by. */
export const STRATEGY_WRITER_AGENT_ID = 'strategy-writer'

/**
 * The writer behind the compile workflow: no tools, no memory, no persona. Every call
 * carries its whole context in the prompt and answers with a structured object.
 */
export const strategyWriter = new Agent({
  id: STRATEGY_WRITER_AGENT_ID,
  name: 'Strategy writer',
  description: 'Turns an agreed strategy brief into a compilable strategy document.',
  instructions: RULEBOOK,
  model: 'openai/gpt-5.6-terra',
})
