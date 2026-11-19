'use client'

import type { RustSignalCatalogue } from '@kyros/domain'

import { useSignalCatalogue } from '@/features/market-detail/use-signal-catalogue'
import { signalDisplayName } from '@/features/signals/signal-cards'
import { type ChatData, useChatData } from '../bindings/chat-data'
import type { WidgetSpec } from '../specs'
import { DataTable } from '../widgets/data-table'
import { WidgetAuthored } from './frames'

type TableSpec = Extract<WidgetSpec, { kind: 'table' }>

/**
 * An authored table. The model names signals by id; the reader sees the English name,
 * which is the server's copy in the exchange catalogue. A chat that is not connected to
 * an exchange has no catalogue, so the id stands.
 */
export function AuthoredTable({ spec }: { spec: TableSpec }) {
  const data = useChatData()
  return data ? (
    <ConnectedAuthoredTable spec={spec} data={data} />
  ) : (
    <TableView spec={spec} catalogue={null} />
  )
}

function ConnectedAuthoredTable({ spec, data }: { spec: TableSpec; data: ChatData }) {
  const { data: catalogue } = useSignalCatalogue(data.apiUrl, data.series)
  return <TableView spec={spec} catalogue={catalogue ?? null} />
}

function TableView({
  spec,
  catalogue,
}: {
  spec: TableSpec
  catalogue: RustSignalCatalogue | null
}) {
  return (
    <WidgetAuthored>
      <DataTable
        label={spec.title ?? 'Table'}
        title={spec.title ?? null}
        columns={spec.columns.map(column => ({ ...column, sortable: true }))}
        rows={spec.rows.map((row, index) => ({
          id: `row-${index}`,
          cells: Object.fromEntries(
            spec.columns.map((column, position) => {
              const value = row[position]
              if (typeof value === 'number')
                return [column.id, { kind: 'number' as const, text: String(value), value }]
              const text = value ?? ''
              return [
                column.id,
                { kind: 'text' as const, text: signalDisplayName(text, catalogue) ?? text },
              ]
            })
          ),
        }))}
      />
    </WidgetAuthored>
  )
}
