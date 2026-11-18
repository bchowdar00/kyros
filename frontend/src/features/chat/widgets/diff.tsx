import { cn } from '@tiny/ui/lib/cn'
import type { ReactNode } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface DiffLine {
  kind: 'context' | 'added' | 'removed'
  text: string
}

export interface DiffProps {
  label: string
  title?: string | null
  meta?: ReactNode
  /** What changed, in the words a person would use: `Strategy v1 → v2`. */
  from: string
  to: string
  lines: readonly DiffLine[]
}

const SIGN: Record<DiffLine['kind'], string> = { context: ' ', added: '+', removed: '−' }

/** Two versions of text or structured items, paired line by line. */
export function Diff({ label, title, meta, from, to, lines }: DiffProps) {
  const added = lines.filter(line => line.kind === 'added').length
  const removed = lines.filter(line => line.kind === 'removed').length

  return (
    <WidgetFrame
      label={label}
      {...(title === undefined ? {} : { title })}
      meta={
        meta ?? (
          <span className="font-mono">
            <span className="text-positive">+{added}</span>{' '}
            <span className="text-negative">−{removed}</span>
          </span>
        )
      }
    >
      <p className="flex items-center gap-sm text-caption text-muted-foreground">
        <span className="truncate">{from}</span>
        <span aria-hidden="true">→</span>
        <span className="truncate text-foreground">{to}</span>
      </p>
      <ol className="-mx-lg overflow-x-auto px-lg font-mono text-label leading-relaxed">
        {lines.map((line, index) => (
          <li
            key={index}
            className={cn('flex gap-md whitespace-pre rounded-xs px-sm', {
              'bg-positive-muted text-foreground': line.kind === 'added',
              'bg-negative-muted text-foreground': line.kind === 'removed',
              'text-muted-foreground': line.kind === 'context',
            })}
          >
            <span
              className={cn('w-sm shrink-0 select-none', {
                'text-positive': line.kind === 'added',
                'text-negative': line.kind === 'removed',
              })}
            >
              <span className="sr-only">
                {line.kind === 'added' ? 'Added ' : line.kind === 'removed' ? 'Removed ' : ''}
              </span>
              <span aria-hidden="true">{SIGN[line.kind]}</span>
            </span>
            <span>{line.text}</span>
          </li>
        ))}
      </ol>
    </WidgetFrame>
  )
}
