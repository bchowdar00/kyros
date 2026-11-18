import { cn } from '@tiny/ui/lib/cn'
import type { ReactNode } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface HeatmapProps {
  label: string
  title?: string
  meta?: ReactNode
  rows: readonly string[]
  columns: readonly string[]
  /** `values[row][column]`. */
  values: readonly (readonly number[])[]
  format: (value: number) => string
  /** Two-hue scale around zero instead of one hue from zero. */
  diverging?: boolean
  rowHeader?: string
}

/** Cell opacity floor so the lightest cell is still visibly a cell. */
const MIN_OPACITY = 0.12

export function Heatmap({
  label,
  title,
  meta,
  rows,
  columns,
  values,
  format,
  diverging = false,
  rowHeader = 'Row',
}: HeatmapProps) {
  const flat = values.flat()
  const peak = Math.max(1e-9, ...flat.map(value => Math.abs(value)))
  const floor = Math.min(0, ...flat)
  const opacity = (value: number) =>
    MIN_OPACITY +
    (1 - MIN_OPACITY) * (diverging ? Math.abs(value) / peak : (value - floor) / (peak - floor || 1))

  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <table className="w-full border-separate border-spacing-xs text-caption">
        <thead>
          <tr>
            <th scope="col" className="text-left font-medium text-muted-foreground">
              <span className="sr-only">{rowHeader}</span>
            </th>
            {columns.map(column => (
              <th key={column} scope="col" className="pb-xs font-medium text-muted-foreground">
                {column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, rowIndex) => (
            <tr key={row}>
              <th scope="row" className="pr-sm text-left font-medium text-muted-foreground">
                {row}
              </th>
              {columns.map((column, columnIndex) => {
                const value = values[rowIndex]?.[columnIndex] ?? 0
                return (
                  <td key={column} className="relative h-2xl rounded-xs text-center">
                    <span
                      aria-hidden="true"
                      className={cn('absolute inset-0 rounded-xs', {
                        'bg-action': !diverging,
                        'bg-positive': diverging && value >= 0,
                        'bg-negative': diverging && value < 0,
                      })}
                      style={{ opacity: opacity(value) }}
                    />
                    <span
                      className={cn('relative font-mono', {
                        // Ink flips on the strongest cells so the value clears contrast on either.
                        'text-background': opacity(value) > 0.7,
                        'text-foreground': opacity(value) <= 0.7,
                      })}
                    >
                      {format(value)}
                    </span>
                  </td>
                )
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </WidgetFrame>
  )
}
