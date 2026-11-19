'use client'

import { cn } from '@kyros/ui/lib/cn'
import { type ReactNode, useRef } from 'react'

import { usePlotWidth } from './plot'
import { ValueSlot } from './value-slot'
import { WidgetFrame } from '../widget-frame'

/** Every chat plot is this tall, the `chat-plot` token: 8rem. */
export const CHAT_PLOT_HEIGHT = 128

export interface ChartShellLegendItem {
  id: string
  label: string
  /** A semantic token, for example `var(--action)`. */
  color: string
  value?: string | undefined
}

export interface ChartShellWindow {
  label: string
  /** Milliseconds of history to show; `null` shows the whole window. */
  ms: number | null
}

export interface ChartShellProps {
  label: string
  title?: string | null | undefined
  /** Header value. A string sits in a slot that never shrinks; a node is used as given. */
  value?: string | ReactNode
  legend?: readonly ChartShellLegendItem[] | undefined
  windows?: readonly ChartShellWindow[] | undefined
  windowIndex?: number
  onWindowChange?: (index: number) => void
  /** One line under the plot, such as axis names. */
  footer?: ReactNode
  /** Draw the same rows empty, at the same height, while the data is on its way. */
  loading?: boolean
  children?: (size: { width: number; height: number }) => ReactNode
}

/**
 * The fixed skeleton every chat chart sits in: a header with a value slot, an optional
 * legend row, the plot box, optional window pills, an optional footer. Every row has a
 * fixed height and every value a slot that only grows, so a tick, a legend value or a
 * feed dropping never moves the text above or below the widget.
 */
export function ChartShell({
  label,
  title,
  value,
  legend,
  windows,
  windowIndex = 0,
  onWindowChange,
  footer,
  loading = false,
  children,
}: ChartShellProps) {
  const body = useRef<HTMLDivElement>(null)
  const width = usePlotWidth(body)
  const meta =
    typeof value === 'string' ? <ValueSlot className="text-foreground">{value}</ValueSlot> : value

  return (
    <WidgetFrame
      label={label}
      {...(title === undefined ? {} : { title })}
      meta={meta}
      aria-busy={loading || undefined}
    >
      {legend !== undefined ? (
        <ul
          aria-label="Series"
          className="flex h-lg flex-nowrap items-center gap-x-lg overflow-hidden text-label"
        >
          {legend.map(item => (
            <li key={item.id} className="flex min-w-0 items-center gap-sm">
              <svg aria-hidden="true" width="12" height="4" className="shrink-0">
                <rect width="12" height="4" rx="2" style={{ fill: item.color }} />
              </svg>
              <span className="truncate text-muted-foreground">{item.label}</span>
              {item.value !== undefined ? (
                <ValueSlot className="text-foreground">{item.value}</ValueSlot>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
      <div ref={body} className="relative h-chat-plot w-full overflow-clip">
        {loading || children === undefined ? (
          <p className="absolute inset-0 m-auto h-fit w-fit text-label text-muted-foreground">
            Preparing…
          </p>
        ) : (
          children({ width, height: CHAT_PLOT_HEIGHT })
        )}
      </div>
      {windows !== undefined && windows.length > 1 ? (
        <fieldset className="flex h-xl items-center gap-xs" disabled={loading}>
          <legend className="sr-only">Window</legend>
          {windows.map((window, index) => (
            <button
              key={window.label}
              type="button"
              aria-pressed={index === windowIndex}
              onClick={() => onWindowChange?.(index)}
              className={cn(
                'rounded-xs px-sm py-xs font-mono text-caption transition-colors focus-visible:outline-2 focus-visible:outline-focus-ring',
                {
                  'bg-surface-raised text-foreground': index === windowIndex,
                  'text-muted-foreground hover:text-foreground': index !== windowIndex,
                }
              )}
            >
              {window.label}
            </button>
          ))}
        </fieldset>
      ) : null}
      {footer !== undefined ? (
        <div className="flex h-lg items-center justify-between text-caption text-muted-foreground">
          {footer}
        </div>
      ) : null}
    </WidgetFrame>
  )
}
