import { cn } from '@tiny/ui/lib/cn'
import type { ReactNode } from 'react'

import { type Stat, StatGridBody } from './stat-grid'
import { ValueSlot } from './value-slot'
import { WidgetFrame } from '../widget-frame'

export interface CardHero {
  value: string
  /** What the hero number is, when the eyebrow does not already say. */
  label?: string
  /** Signed change or distance shown beside the value; its sign picks the tint. */
  delta?: string
  /** Tint the value itself rather than a delta. */
  direction?: 'up' | 'down'
}

export interface CardProps {
  label: string
  /** Top-left identity line, for example the series name. */
  eyebrow: string
  /** Top-right companion, for example a countdown. */
  eyebrowMeta?: ReactNode
  hero: CardHero
  stats: readonly Stat[]
  columns?: 2 | 3
  footer?: ReactNode
}

function deltaTone(delta: string) {
  if (delta.startsWith('+')) return 'text-positive'
  if (delta.startsWith('-') || delta.startsWith('−')) return 'text-negative'
  return 'text-muted-foreground'
}

/**
 * A titled block with one primary value and labelled fields: the market card
 * generalised. Anything with a plot in it is a chart with a header, not a card.
 */
export function Card({ label, eyebrow, eyebrowMeta, hero, stats, columns = 3, footer }: CardProps) {
  return (
    <WidgetFrame label={label} title={eyebrow} meta={eyebrowMeta} className="gap-lg">
      <div className="flex flex-col gap-copy">
        {hero.label ? (
          <span className="text-caption text-muted-foreground">{hero.label}</span>
        ) : null}
        <div className="flex items-baseline gap-md">
          <ValueSlot
            align="start"
            className={cn('text-trade-amount leading-none text-foreground', {
              'text-positive': hero.direction === 'up',
              'text-negative': hero.direction === 'down',
            })}
          >
            {hero.value}
          </ValueSlot>
          {hero.delta ? (
            <ValueSlot align="start" className={cn('text-body', deltaTone(hero.delta))}>
              {hero.delta}
            </ValueSlot>
          ) : null}
        </div>
      </div>
      <StatGridBody stats={stats} columns={columns} />
      {footer ? <div className="text-label text-muted-foreground">{footer}</div> : null}
    </WidgetFrame>
  )
}
