import { cn } from '@kyros/ui/lib/cn'
import type { ReactNode } from 'react'

import { ValueSlot } from './value-slot'
import { WidgetFrame } from '../widget-frame'

export interface Stat {
  id: string
  label: string
  value: string
  /** Signed change text, for example `+3.2¢`; the sign picks the tint. */
  delta?: string
  /** Direction of the value itself, when the value rather than its change is signed. */
  direction?: 'up' | 'down'
}

export interface StatGridProps {
  label: string
  title?: string | null
  meta?: ReactNode
  stats: readonly Stat[]
  columns?: 2 | 3
}

function deltaTone(delta: string) {
  if (delta.startsWith('+')) return 'text-positive'
  if (delta.startsWith('-') || delta.startsWith('−')) return 'text-negative'
  return 'text-muted-foreground'
}

/** Labelled scalars in a grid. The body of a card, or a widget of its own. */
export function StatGridBody({ stats, columns = 2 }: Pick<StatGridProps, 'stats' | 'columns'>) {
  return (
    <dl
      className={cn('grid gap-x-lg gap-y-md', {
        'grid-cols-2': columns === 2,
        'grid-cols-3': columns === 3,
      })}
    >
      {stats.map(stat => (
        <div key={stat.id} className="flex min-w-0 flex-col gap-copy">
          <dt className="truncate text-caption text-muted-foreground">{stat.label}</dt>
          <dd className="flex items-baseline gap-sm">
            <ValueSlot
              align="start"
              className={cn('text-body text-foreground', {
                'text-positive': stat.direction === 'up',
                'text-negative': stat.direction === 'down',
              })}
            >
              {stat.value}
            </ValueSlot>
            {stat.delta ? (
              <ValueSlot align="start" className={cn('text-caption', deltaTone(stat.delta))}>
                {stat.delta}
              </ValueSlot>
            ) : null}
          </dd>
        </div>
      ))}
    </dl>
  )
}

export function StatGrid({ label, title, meta, stats, columns }: StatGridProps) {
  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <StatGridBody stats={stats} {...(columns === undefined ? {} : { columns })} />
    </WidgetFrame>
  )
}
