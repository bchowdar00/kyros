'use client'

import { useEffect, useId, useRef, useState, type FormEvent, type KeyboardEvent } from 'react'
import { cn } from '@tiny/ui/lib/cn'
import { NanoCheckIcon } from '@tiny/ui/icons/nano-check-icon'
import type {
  AskQuestionOutput,
  QuestionKind,
} from '@/mastra/capabilities/strategy/question-catalogue'

interface QuestionCardProps {
  question: AskQuestionOutput
  /** Sends the chosen label as the user's answer. */
  onAnswer: (label: string) => void
  /** "Something else": close the card and let the user type. */
  onDismiss: () => void
}

interface Row {
  label: string
  reason: string
  recommended: boolean
}

/** Options in display order: the recommendation first, then the rest as given. */
function rows(question: AskQuestionOutput): Row[] {
  const all = question.options.map((option, index) => ({
    ...option,
    recommended: index === question.recommended,
  }))
  return [...all.filter(row => row.recommended), ...all.filter(row => !row.recommended)]
}

/** The part of the strategy the question is about, in the user's words. */
const TOPIC: Record<QuestionKind, string> = {
  side: 'Side',
  recurrence: 'Timing',
  entry_style: 'Entry',
  exit: 'Exit',
  stop_time: 'Close',
  custom: 'Detail',
}

/**
 * Teeny's multiple-choice question, pinned above the composer: a decision the agent
 * should not make alone, answered in one keystroke. It sits in the same dark panel
 * the build card uses, with the choices laid out like the build steps so the whole
 * region reads as one surface. Arrows move between the choices, Enter or a click
 * answers, the digits answer directly, Escape (or "Something else") hands control
 * back to the composer. The answer travels as an ordinary user message.
 */
export function QuestionCard({ question, onAnswer, onDismiss }: QuestionCardProps) {
  const options = rows(question)
  const [active, setActive] = useState(0)
  const buttons = useRef<(HTMLButtonElement | null)[]>([])
  const input = useRef<HTMLInputElement>(null)
  const [typing, setTyping] = useState(false)
  const [typed, setTyped] = useState('')
  const headingId = useId()
  useEffect(() => {
    if (typing) input.current?.focus()
  }, [typing])
  // The "something else" row is the last focusable choice.
  const count = options.length + 1

  function focus(index: number) {
    setActive(index)
    buttons.current[index]?.focus()
  }

  function onKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    const digit = Number.parseInt(event.key, 10)
    if (Number.isInteger(digit) && digit >= 1 && digit <= count) {
      event.preventDefault()
      const chosen = options[digit - 1]
      if (chosen) onAnswer(chosen.label)
      else setTyping(true)
      return
    }
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault()
        focus((active + 1) % count)
        break
      case 'ArrowUp':
        event.preventDefault()
        focus((active - 1 + count) % count)
        break
      case 'Escape':
        event.preventDefault()
        onDismiss()
        break
      default:
    }
  }

  function submitTyped(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const answer = typed.trim()
    if (answer) onAnswer(answer)
  }

  function onInputKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key !== 'Escape') return
    event.preventDefault()
    setTyping(false)
    focus(options.length)
  }

  const rowClass =
    'group flex min-h-touch w-full items-center gap-sm rounded-sm px-xs text-left text-label transition-colors motion-reduce:transition-none focus-visible:outline-2 focus-visible:outline-focus-ring'

  return (
    <section aria-labelledby={headingId} className="px-sm pt-sm">
      <div className="relative overflow-clip rounded-md border border-highlight bg-surface text-foreground">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute left-1/2 top-0 size-public-strategy-glow -translate-x-1/2 -translate-y-1/2 rounded-pill bg-action opacity-15 blur-public-strategy-glow"
        />
        <div className="relative flex flex-col gap-md p-lg">
          <div className="flex items-center justify-between gap-md">
            <h3 id={headingId} className="text-label font-medium text-foreground">
              Teeny needs a decision
            </h3>
            <span className="rounded-pill border border-highlight px-sm py-xs text-caption text-muted-foreground">
              {TOPIC[question.kind]}
            </span>
          </div>
          <p className="max-w-prose text-body text-muted-foreground">{question.question}</p>
          <div aria-hidden="true" className="relative h-px w-full bg-highlight">
            <div className="absolute inset-y-0 left-0 w-1/3 bg-action" />
          </div>
          <fieldset className="flex flex-col">
            <legend className="sr-only">{question.question}</legend>
            {options.map((option, index) => {
              const current = index === active
              return (
                <button
                  key={option.label}
                  type="button"
                  ref={element => {
                    buttons.current[index] = element
                  }}
                  tabIndex={current ? 0 : -1}
                  onKeyDown={onKeyDown}
                  onClick={() => onAnswer(option.label)}
                  onFocus={() => setActive(index)}
                  className={cn({
                    [rowClass]: true,
                    'text-foreground': current,
                    'text-muted-foreground hover:text-foreground': !current,
                  })}
                >
                  <span
                    aria-hidden="true"
                    className="relative flex size-md shrink-0 items-center justify-center"
                  >
                    {current ? (
                      <NanoCheckIcon className="size-md text-positive" />
                    ) : (
                      <span className="size-xs rounded-full bg-muted-foreground/60 group-hover:bg-foreground" />
                    )}
                  </span>
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className="flex items-center gap-sm">
                      <span>{option.label}</span>
                      {option.recommended ? (
                        <span className="rounded-pill border border-positive-muted px-sm text-caption text-positive">
                          Recommended
                        </span>
                      ) : null}
                    </span>
                    {option.reason ? (
                      <span className="text-caption text-muted-foreground">{option.reason}</span>
                    ) : null}
                  </span>
                  <span
                    aria-hidden="true"
                    className="shrink-0 font-mono text-caption text-muted-foreground/60"
                  >
                    {index + 1}
                  </span>
                </button>
              )
            })}
            <button
              type="button"
              ref={element => {
                buttons.current[options.length] = element
              }}
              tabIndex={active === options.length ? 0 : -1}
              onKeyDown={onKeyDown}
              onClick={() => setTyping(true)}
              onFocus={() => setActive(options.length)}
              className={cn({
                [rowClass]: true,
                'text-foreground': active === options.length,
                'text-muted-foreground hover:text-foreground': active !== options.length,
              })}
            >
              <span
                aria-hidden="true"
                className="flex size-md shrink-0 items-center justify-center"
              >
                <span className="text-caption">+</span>
              </span>
              <span className="flex-1">Something else, I’ll type it</span>
              <span
                aria-hidden="true"
                className="shrink-0 font-mono text-caption text-muted-foreground/60"
              >
                {count}
              </span>
            </button>
          </fieldset>
          {typing ? (
            <form onSubmit={submitTyped} className="flex items-center gap-sm">
              <input
                ref={input}
                type="text"
                value={typed}
                onChange={event => setTyped(event.target.value)}
                onKeyDown={onInputKeyDown}
                aria-label="Your answer"
                placeholder="Say how you want it to work…"
                maxLength={500}
                className="min-h-touch min-w-0 flex-1 rounded-sm border border-highlight bg-background/50 px-sm text-body text-foreground placeholder:text-muted-foreground/60 focus-visible:outline-2 focus-visible:outline-focus-ring"
              />
              <button
                type="submit"
                disabled={typed.trim().length === 0}
                className="min-h-touch shrink-0 rounded-sm border border-highlight px-md text-label text-foreground disabled:text-muted-foreground/60 focus-visible:outline-2 focus-visible:outline-focus-ring"
              >
                Send answer
              </button>
            </form>
          ) : null}
          <p className="text-caption text-muted-foreground/60">
            {typing
              ? 'Enter sends your answer, Esc goes back to the choices'
              : 'Press a number to answer, or Esc to close and type below'}
          </p>
        </div>
      </div>
    </section>
  )
}
