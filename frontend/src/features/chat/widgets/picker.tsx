'use client'

import { Button } from '@kyros/ui/components/button'
import { NanoCheckIcon } from '@kyros/ui/icons/nano-check-icon'
import { cn } from '@kyros/ui/lib/cn'
import { useId, useState } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface PickerOption {
  id: string
  title: string
  description?: string
  /** A short figure on the right, for example a price or a count. */
  meta?: string
}

export interface PickerProps {
  label: string
  title?: string
  /** The question the options answer. */
  prompt: string
  options: readonly PickerOption[]
  defaultOptionId?: string
  confirmLabel?: string
  onSelect: (optionId: string) => void
}

/** Choose one of a few options and send the choice back into the conversation. */
export function Picker({
  label,
  title,
  prompt,
  options,
  defaultOptionId,
  confirmLabel = 'Continue',
  onSelect,
}: PickerProps) {
  const [selected, setSelected] = useState<string | undefined>(defaultOptionId)
  const [sent, setSent] = useState<PickerOption | null>(null)
  const groupName = useId()

  if (sent) {
    return (
      <WidgetFrame label={label} {...(title === undefined ? {} : { title })}>
        <output className="flex items-center gap-sm text-body text-foreground">
          <NanoCheckIcon className="size-md text-positive" />
          {sent.title}
        </output>
      </WidgetFrame>
    )
  }

  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })}>
      <fieldset className="flex flex-col gap-sm">
        <legend className="mb-sm text-body text-foreground">{prompt}</legend>
        {options.map(option => {
          const active = option.id === selected
          return (
            <label
              key={option.id}
              className={cn(
                'flex cursor-pointer items-start gap-md rounded-md border-[0.5px] px-lg py-md transition-colors has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-focus-ring',
                {
                  'border-foreground/60 bg-surface-raised': active,
                  'border-border-subtle hover:bg-surface': !active,
                }
              )}
            >
              <input
                type="radio"
                name={groupName}
                value={option.id}
                checked={active}
                onChange={() => setSelected(option.id)}
                className="sr-only"
              />
              <span
                aria-hidden="true"
                className={cn(
                  'mt-xs flex size-md shrink-0 items-center justify-center rounded-pill border-[0.5px]',
                  {
                    'border-foreground bg-foreground text-background': active,
                    'border-border': !active,
                  }
                )}
              >
                {active ? <NanoCheckIcon className="size-sm" /> : null}
              </span>
              <span className="flex min-w-0 flex-1 flex-col gap-copy">
                <span className="flex items-baseline justify-between gap-md">
                  <span className="text-body text-foreground">{option.title}</span>
                  {option.meta ? (
                    <span className="shrink-0 font-mono text-label text-muted-foreground">
                      {option.meta}
                    </span>
                  ) : null}
                </span>
                {option.description ? (
                  <span className="text-label text-muted-foreground">{option.description}</span>
                ) : null}
              </span>
            </label>
          )
        })}
      </fieldset>
      <Button
        size="medium"
        disabled={selected === undefined}
        onClick={() => {
          const option = options.find(candidate => candidate.id === selected)
          if (!option) return
          setSent(option)
          onSelect(option.id)
        }}
      >
        {confirmLabel}
      </Button>
    </WidgetFrame>
  )
}
