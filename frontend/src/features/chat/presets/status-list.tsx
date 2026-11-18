import { DataTable, type TableCell } from '../widgets/data-table'

export interface StatusItem {
  id: string
  label: string
  /** The live value the rule is watching, already formatted. */
  value: string
  state: 'on' | 'off' | 'warn'
  stateLabel: string
  /** How long it has been in this state, already formatted. */
  since?: string
}

export interface StatusListProps {
  title: string
  items: readonly StatusItem[]
}

const COLUMNS = [
  { id: 'label', label: 'Rule' },
  { id: 'value', label: 'Value', align: 'right' as const },
  { id: 'state', label: 'State' },
  { id: 'since', label: 'For', align: 'right' as const },
]

/** Rule status, watches, checklists: a label, a live value and a pip, as a table preset. */
export function StatusList({ title, items }: StatusListProps) {
  return (
    <DataTable
      label={`${title} status`}
      title={title}
      meta={`${items.filter(item => item.state === 'on').length} of ${items.length} met`}
      columns={COLUMNS}
      dense
      rows={items.map(item => ({
        id: item.id,
        cells: {
          label: { kind: 'text', text: item.label },
          value: { kind: 'number', text: item.value, value: 0 },
          state: { kind: 'pip', state: item.state, text: item.stateLabel },
          since: { kind: 'text', text: item.since ?? '', muted: true } satisfies TableCell,
        },
      }))}
    />
  )
}
