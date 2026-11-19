'use client'

import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { cn } from '@kyros/ui/lib/cn'
import { type ReactNode, useState } from 'react'

import { cellSortValue, type TableCell, TableCellContent } from './table-cells'
import { WidgetFrame } from '../widget-frame'

export type { TableCell }

export interface TableColumn {
  id: string
  label: string
  align?: 'left' | 'right'
  sortable?: boolean
}

export interface TableRow {
  id: string
  cells: Record<string, TableCell>
}

export interface DataTableProps {
  label: string
  title?: string | null
  meta?: ReactNode
  columns: readonly TableColumn[]
  rows: readonly TableRow[]
  defaultSort?: { column: string; direction: 'ascending' | 'descending' }
  /** Tighter rows for long lists such as a fill log. */
  dense?: boolean
}

type Sort = NonNullable<DataTableProps['defaultSort']>

function compare(a: number | string, b: number | string) {
  if (typeof a === 'number' && typeof b === 'number') return a - b
  return String(a).localeCompare(String(b))
}

export function DataTable({
  label,
  title,
  meta,
  columns,
  rows,
  defaultSort,
  dense = false,
}: DataTableProps) {
  const [sort, setSort] = useState<Sort | undefined>(defaultSort)
  const sorted = sort
    ? rows.toSorted((first, second) => {
        const a = first.cells[sort.column]
        const b = second.cells[sort.column]
        const order = compare(a ? cellSortValue(a) : '', b ? cellSortValue(b) : '')
        return sort.direction === 'ascending' ? order : -order
      })
    : rows

  function toggleSort(column: string) {
    setSort(current =>
      current?.column === column
        ? { column, direction: current.direction === 'ascending' ? 'descending' : 'ascending' }
        : { column, direction: 'descending' }
    )
  }

  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <div className="-mx-lg overflow-x-auto px-lg">
        <table className="w-full border-collapse text-label">
          <thead>
            <tr className="border-b-[0.5px] border-border-subtle">
              {columns.map(column => (
                <th
                  key={column.id}
                  scope="col"
                  aria-sort={sort?.column === column.id ? sort.direction : undefined}
                  className={cn(
                    'px-sm pb-sm font-medium text-muted-foreground first:pl-0 last:pr-0',
                    {
                      'text-right': column.align === 'right',
                      'text-left': column.align !== 'right',
                    }
                  )}
                >
                  {column.sortable ? (
                    <button
                      type="button"
                      onClick={() => toggleSort(column.id)}
                      className="inline-flex items-center gap-xs rounded-xs hover:text-foreground focus-visible:outline-2 focus-visible:outline-focus-ring"
                    >
                      {column.label}
                      <NanoChevronIcon
                        className={cn('size-md transition-transform', {
                          'opacity-0': sort?.column !== column.id,
                          'rotate-180':
                            sort?.column === column.id && sort.direction === 'ascending',
                        })}
                      />
                    </button>
                  ) : (
                    column.label
                  )}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {sorted.map(row => (
              <tr key={row.id} className="border-b-[0.5px] border-border-subtle/50 last:border-0">
                {columns.map(column => {
                  const cell = row.cells[column.id]
                  return (
                    <td
                      key={column.id}
                      className={cn('px-sm text-foreground first:pl-0 last:pr-0', {
                        'py-sm': !dense,
                        'py-xs': dense,
                        'text-right': column.align === 'right',
                      })}
                    >
                      {cell ? <TableCellContent cell={cell} /> : null}
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </WidgetFrame>
  )
}
