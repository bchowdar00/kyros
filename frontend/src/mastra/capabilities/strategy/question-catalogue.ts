import * as z from 'zod/mini'
import { described } from './describe'
import { BTC_SIDES, type Sides, capitalize } from './sides'

/**
 * The questions Teeny may ask while filling a brief, as a catalogue keyed by the brief
 * slot that is ambiguous. A catalogue kind carries canned short labels so the answers
 * are consistent, one keystroke, and applied to the brief by kind rather than
 * re-interpreted. `custom` is the escape hatch for a genuinely new ambiguity; its
 * labels are capped so the card stays scannable.
 */

export const QUESTION_KINDS = [
  'side',
  'recurrence',
  'entry_style',
  'exit',
  'stop_time',
  'custom',
] as const
export type QuestionKind = (typeof QUESTION_KINDS)[number]

export interface QuestionOption {
  label: string
  /** One short line on why, shown muted under the label; empty for none. */
  reason: string
}

export interface CatalogueEntry {
  question: string
  options: QuestionOption[]
}

export const CATALOGUE: Record<Exclude<QuestionKind, 'custom'>, CatalogueEntry> = {
  side: {
    question: 'Which side should it buy?',
    options: [
      { label: 'Up', reason: '' },
      { label: 'Down', reason: '' },
      { label: 'Whichever side qualifies', reason: 'The rule picks the side when it fires' },
    ],
  },
  recurrence: {
    question: 'How often should it act?',
    options: [
      { label: 'Once per round', reason: 'The first time the rule is true' },
      { label: 'Every time', reason: 'Each time the rule becomes true, paced by a cooldown' },
      { label: 'On a timer', reason: 'At a fixed interval while the rule holds' },
    ],
  },
  entry_style: {
    question: 'How should it enter?',
    options: [
      { label: 'Take the market', reason: 'Fills at once, a cent through the book' },
      { label: 'Rest a limit order', reason: 'Waits on the book and follows it' },
    ],
  },
  exit: {
    question: 'How should it get out?',
    options: [
      { label: 'Hold to settlement', reason: '' },
      { label: 'Sell at a profit target', reason: '' },
      { label: 'Cut at a stop', reason: '' },
      { label: 'Target and stop', reason: '' },
    ],
  },
  stop_time: {
    question: 'When should it stop before the close?',
    options: [
      { label: '10 seconds', reason: '' },
      { label: '20 seconds', reason: '' },
      { label: '30 seconds', reason: '' },
    ],
  },
}

/** The most words a custom label may run to; a question is answered at a glance. */
export const MAX_LABEL_WORDS = 6
export const MAX_LABEL_CHARS = 40
export const MIN_OPTIONS = 2
export const MAX_OPTIONS = 4

export const AskQuestionInputSchema = z.object({
  kind: described(
    z.enum(QUESTION_KINDS),
    'The brief slot in question. A catalogue kind brings its own options; `custom` needs `options`.'
  ),
  question: described(
    z.string().check(z.maxLength(200)),
    'The question in plain trading language, one line. Catalogue kinds may leave their default by passing an empty string.'
  ),
  options: described(
    z.array(
      z.object({
        label: z.string().check(z.minLength(1), z.maxLength(MAX_LABEL_CHARS)),
        reason: z.string().check(z.maxLength(120)),
      })
    ),
    `Custom kinds only: ${MIN_OPTIONS} to ${MAX_OPTIONS} readings, each label at most ${MAX_LABEL_WORDS} words. Ignored for catalogue kinds.`
  ),
  recommended: described(
    z.nullable(z.int()),
    'Index of the option you would pick, or null when none is better. Shown first as the recommendation.'
  ),
})
export type AskQuestionInput = z.infer<typeof AskQuestionInputSchema>

export const AskQuestionOutputSchema = z.object({
  status: z.literal('awaiting_answer'),
  kind: z.enum(QUESTION_KINDS),
  question: z.string(),
  options: z.array(z.object({ label: z.string(), reason: z.string() })),
  recommended: z.nullable(z.int()),
})
export type AskQuestionOutput = z.infer<typeof AskQuestionOutputSchema>

export type AskQuestionResult = AskQuestionOutput | { status: 'invalid'; problem: string }

function wordCount(text: string) {
  return text.trim().split(/\s+/).filter(Boolean).length
}

/** The side question with the market's own labels: Up and Down, or Home and Away. */
export function sideEntry(sides: Sides): CatalogueEntry {
  return {
    question: CATALOGUE.side.question,
    options: CATALOGUE.side.options.map((option, index) =>
      index === 0
        ? { ...option, label: capitalize(sides.quoted) }
        : index === 1
          ? { ...option, label: capitalize(sides.other) }
          : option
    ),
  }
}

/**
 * Resolve the question the card will show, or say why the tool call is malformed.
 * `sides` names the market's sides for the side question; BTC's when not given.
 */
export function resolveQuestion(
  input: AskQuestionInput,
  sides: Sides = BTC_SIDES
): AskQuestionResult {
  const entry =
    input.kind === 'custom'
      ? null
      : input.kind === 'side'
        ? sideEntry(sides)
        : CATALOGUE[input.kind]
  const options = entry ? entry.options : input.options
  if (!entry) {
    if (options.length < MIN_OPTIONS || options.length > MAX_OPTIONS)
      return {
        status: 'invalid',
        problem: `A custom question needs ${MIN_OPTIONS} to ${MAX_OPTIONS} options.`,
      }
    const long = options.find(option => wordCount(option.label) > MAX_LABEL_WORDS)
    if (long)
      return {
        status: 'invalid',
        problem: `Option "${long.label}" is longer than ${MAX_LABEL_WORDS} words; shorten it.`,
      }
    if (!input.question.trim())
      return { status: 'invalid', problem: 'A custom question needs its question text.' }
  }
  const question = input.question.trim() || entry?.question || ''
  const recommended =
    input.recommended !== null && input.recommended >= 0 && input.recommended < options.length
      ? input.recommended
      : null
  return { status: 'awaiting_answer', kind: input.kind, question, options, recommended }
}
