import type { UIMessage } from 'ai'
import * as z from 'zod/mini'
import {
  AskQuestionOutputSchema,
  type AskQuestionOutput,
} from '@/mastra/capabilities/strategy/question-catalogue'

export const QUESTION_PART = 'tool-askQuestion'

export interface AskedQuestion {
  /** The tool call id: stable across renders, used to dismiss. */
  id: string
  output: AskQuestionOutput
}

/** Every question a message carries, in part order. */
export function questionsIn(message: UIMessage): AskedQuestion[] {
  const asked: AskedQuestion[] = []
  for (const part of message.parts) {
    if (part.type !== QUESTION_PART || !('state' in part) || part.state !== 'output-available')
      continue
    const output = z.safeParse(AskQuestionOutputSchema, part.output)
    if (output.success) asked.push({ id: part.toolCallId, output: output.data })
  }
  return asked
}

/**
 * The question Kyros is waiting on: the newest one in the last message, which must be
 * Kyros's (a later user message is the answer). Dismissed questions are typed answers.
 */
export function openQuestion(messages: readonly UIMessage[], dismissed: ReadonlySet<string>) {
  const last = messages.at(-1)
  if (!last || last.role !== 'assistant') return undefined
  return questionsIn(last)
    .filter(question => !dismissed.has(question.id))
    .at(-1)
}
