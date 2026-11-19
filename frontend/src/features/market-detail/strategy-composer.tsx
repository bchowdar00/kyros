'use client'

import { cn } from '@kyros/ui/lib/cn'
import { IconButton } from '@kyros/ui/components/icon-button'
import { MiniXIcon } from '@kyros/ui/icons/mini-x-icon'
import { PlusIcon } from '@kyros/ui/icons/plus-icon'
import { useEffect, useRef, useState, type FormEvent } from 'react'

import type { StrategyComposerData } from './types'

export type StrategyComposerSubmitAction = (prompt: string) => void

/** Shown in place of the composer's tip when LLM authoring is off for the series. */
const AUTHORING_UNAVAILABLE_NOTICE =
  'The assistant writes Bitcoin strategies for now. Pick a ready-made strategy to run on this game.'

export interface StrategyComposerProps extends Partial<StrategyComposerData> {
  focusInput?: boolean
  embedded?: boolean
  busy?: boolean
  showTip?: boolean
  /** The assistant only authors Bitcoin strategies; on any other series it is turned off. */
  authoringUnavailable?: boolean
  onStop?: () => void
  onAddContext?: () => void
  onSubmit?: StrategyComposerSubmitAction
}

export function StrategyComposer({
  embedded = false,
  focusInput = false,
  busy = false,
  showTip = true,
  authoringUnavailable = false,
  onStop,
  addContextLabel = 'Add strategy context',
  dismissTipLabel = 'Dismiss strategy tip',
  inputLabel = 'Strategy instructions',
  onAddContext,
  onSubmit,
  placeholder = 'Write your own sentence…',
  submitLabel = 'Create strategy',
  tipLinkLabel = 'signals',
  tipPrefix = 'Tip: your strategy can reference available',
}: StrategyComposerProps) {
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (focusInput && !authoringUnavailable) input.current?.focus({ preventScroll: true })
  }, [focusInput, authoringUnavailable])
  const [prompt, setPrompt] = useState('')
  const [isTipVisible, setIsTipVisible] = useState(true)
  const canSubmit =
    !busy && !authoringUnavailable && onSubmit !== undefined && prompt.trim().length > 0

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()

    const submittedPrompt = prompt.trim()
    if (authoringUnavailable || busy || submittedPrompt.length === 0 || onSubmit === undefined) {
      return
    }

    onSubmit(submittedPrompt)
    setPrompt('')
  }

  return (
    <form
      aria-label="Strategy composer"
      className={cn('pointer-events-auto relative w-full overflow-clip', {
        'rounded-composer border-2 border-foreground bg-background': !embedded,
      })}
      onSubmit={handleSubmit}
    >
      <div className="relative p-xs">
        <div className="flex min-h-app-nav items-center gap-md rounded-md border-[1.5px] border-foreground bg-background px-sm has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-focus-ring">
          <IconButton
            aria-label={addContextLabel}
            className="relative border-transparent bg-surface text-foreground hover:bg-surface-raised disabled:border-transparent after:absolute after:-inset-copy after:content-['']"
            disabled={onAddContext === undefined}
            onClick={onAddContext}
            size="medium"
            variant="ghost"
          >
            <PlusIcon />
          </IconButton>

          <input
            ref={input}
            aria-label={inputLabel}
            className="min-w-0 flex-1 bg-transparent text-heading-sm font-medium text-foreground outline-none placeholder:text-muted-foreground disabled:cursor-not-allowed"
            disabled={authoringUnavailable}
            onChange={event => setPrompt(event.currentTarget.value)}
            placeholder={placeholder}
            type="text"
            value={prompt}
          />

          <IconButton
            aria-label={busy ? 'Stop response' : submitLabel}
            className="relative border-foreground bg-foreground text-background hover:border-action hover:bg-action hover:text-on-action after:absolute after:-inset-copy after:content-['']"
            disabled={authoringUnavailable || (busy ? onStop === undefined : !canSubmit)}
            size="medium"
            type={busy ? 'button' : 'submit'}
            onClick={busy ? onStop : undefined}
            variant="ghost"
          >
            <span aria-hidden="true">{busy ? '■' : '↑'}</span>
          </IconButton>
        </div>
      </div>

      {authoringUnavailable ? (
        <p className="relative px-md pt-sm pb-md text-label font-regular text-muted-foreground">
          {AUTHORING_UNAVAILABLE_NOTICE}
        </p>
      ) : showTip && isTipVisible ? (
        <div className="relative flex items-center justify-between px-md pt-sm pb-md">
          <p className="truncate text-label font-regular text-muted-foreground">
            {tipPrefix}{' '}
            <a
              className="underline underline-offset-2 focus-visible:outline-2 focus-visible:outline-focus-ring"
              href="#signals"
            >
              {tipLinkLabel}
            </a>
          </p>

          <IconButton
            aria-label={dismissTipLabel}
            className="relative size-md text-foreground after:absolute after:-top-2xl after:-right-sm after:-bottom-sm after:-left-2xl after:content-['']"
            onClick={() => setIsTipVisible(false)}
            size="small"
            variant="ghost"
          >
            <MiniXIcon className="size-md" />
          </IconButton>
        </div>
      ) : null}
    </form>
  )
}
