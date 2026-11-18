import { buildLinePath } from '@tiny/ui/charts/lib/path'
import { createLinearScale } from '@tiny/ui/charts/lib/scale'
import { cn } from '@tiny/ui/lib/cn'

export type TableCell =
  | { kind: 'text'; text: string; muted?: boolean }
  | { kind: 'number'; text: string; value: number; tone?: 'positive' | 'negative' }
  | { kind: 'sparkline'; values: readonly number[]; tone?: 'positive' | 'negative' }
  | { kind: 'pip'; state: 'on' | 'off' | 'warn'; text: string }

const SPARK_WIDTH = 56
const SPARK_HEIGHT = 14

/** The value a cell sorts by: numbers by value, everything else by its text. */
export function cellSortValue(cell: TableCell): number | string {
  switch (cell.kind) {
    case 'number':
      return cell.value
    case 'sparkline':
      return cell.values.at(-1) ?? 0
    case 'pip':
      return cell.state === 'on' ? 2 : cell.state === 'warn' ? 1 : 0
    default:
      return cell.text
  }
}

export function TableCellContent({ cell }: { cell: TableCell }) {
  switch (cell.kind) {
    case 'text':
      return <span className={cell.muted ? 'text-muted-foreground' : undefined}>{cell.text}</span>
    case 'number':
      return (
        <span
          className={cn('font-mono', {
            'text-positive': cell.tone === 'positive',
            'text-negative': cell.tone === 'negative',
          })}
        >
          {cell.text}
        </span>
      )
    case 'sparkline':
      return <Sparkline values={cell.values} tone={cell.tone} />
    case 'pip':
      return (
        <span className="inline-flex items-center gap-sm">
          <span
            aria-hidden="true"
            className={cn('size-sm rounded-pill', {
              'bg-positive': cell.state === 'on',
              'bg-warning': cell.state === 'warn',
              'bg-muted-foreground/50': cell.state === 'off',
            })}
          />
          {cell.text}
        </span>
      )
  }
}

function Sparkline({
  values,
  tone,
}: {
  values: readonly number[]
  tone?: 'positive' | 'negative' | undefined
}) {
  const xScale = createLinearScale([0, Math.max(1, values.length - 1)], [1, SPARK_WIDTH - 1])
  const yScale = createLinearScale(
    [Math.min(...values), Math.max(...values)],
    [SPARK_HEIGHT - 1, 1]
  )
  const path = buildLinePath(
    values.map((value, index) => ({ x: xScale(index), y: yScale(value) })),
    'monotone'
  )
  return (
    <svg
      aria-hidden="true"
      width={SPARK_WIDTH}
      height={SPARK_HEIGHT}
      className={cn('inline-block align-middle', {
        'stroke-positive': tone === 'positive',
        'stroke-negative': tone === 'negative',
        'stroke-foreground': tone === undefined,
      })}
    >
      <path d={path} fill="none" strokeWidth={1.5} strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  )
}
