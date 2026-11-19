'use client'

import { cn } from '@kyros/ui/lib/cn'
import { type Ref, useState } from 'react'

const DISTANCES_BPS = [10, 20, 5] as const
const TIMES_LEFT = ['two minutes', 'one minute', 'thirty seconds'] as const

const chipClasses =
  'cursor-pointer rounded-xs underline decoration-[3px] underline-offset-[6px] transition-colors hover:bg-action-muted focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'

function next<Option>(options: readonly Option[], current: number) {
  return (current + 1) % options.length
}

export interface StrategySentenceProps {
  /** The writer is already working on something; the sentence can still be edited. */
  busy?: boolean
  /** Lets the rail return focus here when a chat closes. */
  headingRef?: Ref<HTMLHeadingElement>
  onBuild: (sentence: string) => void
}

/**
 * A strategy as one sentence. Each underlined word is a choice; the sentence on screen is,
 * word for word, what the strategy writer is asked to build.
 */
export function StrategySentence({ busy = false, headingRef, onBuild }: StrategySentenceProps) {
  const [up, setUp] = useState(true)
  const [distance, setDistance] = useState(0)
  const [time, setTime] = useState(0)
  const side = up ? 'Up' : 'Down'
  const direction = up ? 'above' : 'below'
  const distanceText = `${DISTANCES_BPS[distance] ?? DISTANCES_BPS[0]} basis points`
  const timeText = `${TIMES_LEFT[time] ?? TIMES_LEFT[0]} left`
  const sentence = `Buy ${side} when BTC is ${distanceText} ${direction} the strike with ${timeText}.`

  return (
    <div className="flex flex-col gap-md">
      {/* The label is the sentence exactly; the buttons inside would otherwise pad it. */}
      <h2
        aria-label={sentence}
        className="text-balance font-display text-display-md font-bold tracking-display text-foreground outline-none"
        ref={headingRef}
        tabIndex={-1}
      >
        Buy{' '}
        <button
          title="Change the side"
          className={cn(chipClasses, up ? 'text-positive' : 'text-foreground')}
          onClick={() => setUp(current => !current)}
          type="button"
        >
          {side}
        </button>{' '}
        when BTC is{' '}
        <button
          title="Change the distance"
          className={cn(chipClasses, 'text-positive')}
          onClick={() => setDistance(current => next(DISTANCES_BPS, current))}
          type="button"
        >
          {distanceText}
        </button>{' '}
        {direction} the strike with{' '}
        <button
          title="Change the time"
          className={cn(chipClasses, 'text-positive')}
          onClick={() => setTime(current => next(TIMES_LEFT, current))}
          type="button"
        >
          {timeText}
        </button>
        .
      </h2>
      <p className="text-body text-muted-foreground">
        Click an underlined word to change it, or write your own sentence below.
      </p>
      <div className="flex flex-wrap items-center gap-lg">
        <button
          className="rounded-md bg-action px-xl py-md font-display text-heading-sm font-bold text-on-action transition-colors hover:bg-foreground hover:text-background focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring active:translate-y-px disabled:cursor-not-allowed disabled:bg-surface disabled:text-muted-foreground"
          disabled={busy}
          onClick={() => onBuild(sentence)}
          type="button"
        >
          Build this strategy
        </button>
        <span className="max-w-content-copy text-label text-muted-foreground">
          Claude writes it. The compiler checks it before any money moves.
        </span>
      </div>
    </div>
  )
}
