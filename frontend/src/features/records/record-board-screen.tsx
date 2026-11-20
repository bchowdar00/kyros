import { cents, createMoneyFormatter } from '@kyros/money'
import { cn } from '@kyros/ui/lib/cn'

import type { NamedRecordBoardRow, RecordBoardView } from './record-board-loader'

export interface RecordBoardScreenProps {
  /** Block explorer origin the transaction and contract links go to. */
  explorerUrl: string
  view: RecordBoardView
}

const money = createMoneyFormatter('en-US')

const utcTime = new Intl.DateTimeFormat('en-US', {
  day: 'numeric',
  hour: '2-digit',
  hourCycle: 'h23',
  minute: '2-digit',
  month: 'short',
  timeZone: 'UTC',
})

const seriesLabels: Record<string, string> = {
  'btc-15m': 'BTC 15m',
  'btc-5m': 'BTC 5m',
  'nba-h2h': 'NBA game',
}

const focusRingClasses =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'
const linkClasses = `rounded-xs text-foreground underline underline-offset-2 transition-colors hover:text-muted-foreground ${focusRingClasses}`
const headClasses = 'px-md py-md text-label font-semibold text-muted-foreground'
const numberClasses = 'px-md py-lg text-right align-middle font-mono text-body text-foreground'

/** `0x1234…abcd`: enough of an id or address to match it on the explorer. */
function short(hex: string): string {
  return `${hex.slice(0, 6)}…${hex.slice(-4)}`
}

/** A block time as `Nov 20, 08:25 UTC`. The explorer's stamps are UTC and are shown as UTC. */
function atUtc(iso: string): string {
  const at = new Date(iso)
  // Formatting an unreadable time throws; one odd stamp must not take the page down.
  return Number.isNaN(at.getTime()) ? 'Time not given' : `${utcTime.format(at)} UTC`
}

function under(base: string, path: string): string {
  return new URL(path, base.endsWith('/') ? base : `${base}/`).href
}

function plural(count: number, one: string, many: string): string {
  return `${money.count(count)} ${count === 1 ? one : many}`
}

function rowName(row: NamedRecordBoardRow): string {
  return row.name ?? `Strategy ${short(row.deploymentId)}`
}

function rowDetail(row: NamedRecordBoardRow): string {
  const parts: string[] = []
  if (row.series !== undefined) parts.push(seriesLabels[row.series] ?? row.series)
  if (row.budgetCents !== undefined) parts.push(`${money.usd(cents(row.budgetCents))} budget`)
  return parts.join(' · ')
}

function BoardRow({
  explorerUrl,
  rank,
  row,
}: {
  explorerUrl: string
  rank: number
  row: NamedRecordBoardRow
}) {
  const name = rowName(row)
  const detail = rowDetail(row)

  return (
    <tr className="border-b border-border-subtle last:border-b-0">
      <td className="py-lg pl-xl pr-md align-middle font-mono text-body text-muted-foreground">
        {rank}
      </td>
      <th className="px-md py-lg text-left align-middle font-regular" scope="row">
        <span className="block font-display text-heading-sm font-bold text-foreground">{name}</span>
        {detail === '' ? null : (
          <span className="block text-caption text-muted-foreground">{detail}</span>
        )}
      </th>
      <td className={numberClasses}>{money.count(row.rounds)}</td>
      <td className={numberClasses}>{money.count(row.wonRounds)}</td>
      <td
        className={cn(
          numberClasses,
          'font-medium',
          row.pnlCents > 0 && 'text-positive',
          row.pnlCents < 0 && 'text-negative'
        )}
      >
        {money.usd(cents(row.pnlCents), { signDisplay: 'exceptZero' })}
      </td>
      <td className="py-lg pl-md pr-xl text-right align-middle text-caption text-muted-foreground">
        <span className="block font-mono">{atUtc(row.lastSealedAt)}</span>
        <a
          className={linkClasses}
          href={under(explorerUrl, `tx/${row.lastTransactionHash}`)}
          rel="noopener noreferrer"
          target="_blank"
        >
          View on Arc<span className="sr-only">: the newest sealed round of {name}</span>
        </a>
      </td>
    </tr>
  )
}

/**
 * The public record board: every strategy that has sealed a round, added up from the
 * contract's transactions on Arc. Nothing on it comes from the exchange except a name.
 */
export function RecordBoardScreen({ explorerUrl, view }: RecordBoardScreenProps) {
  return (
    <main className="mx-auto w-full max-w-[90rem] px-4xl py-4xl">
      <h1 className="mb-md font-display text-display-lg font-bold tracking-display text-foreground">
        Records
      </h1>
      <p className="mb-xl max-w-prose text-body text-muted-foreground">
        Every round a strategy trades is sealed on Arc once Gemini&apos;s result is in. This page is
        added up from those transactions, not from our server&apos;s memory: each line links to the
        chain, and the server supplies only a strategy&apos;s name and market. Results are in
        pretend money.
      </p>

      {view.status === 'unavailable' ? (
        <output className="block text-body text-muted-foreground">
          The Arc explorer is not answering right now, so this page cannot be built. Reload in a
          minute.
        </output>
      ) : (
        <>
          {view.rows.length === 0 ? (
            <p className="text-body text-muted-foreground">No round has been sealed yet.</p>
          ) : (
            <>
              <p className="mb-md text-label font-medium text-foreground">
                {[
                  plural(view.totals.strategies, 'strategy', 'strategies'),
                  `${plural(view.totals.rounds, 'round', 'rounds')} sealed`,
                  `${money.count(view.totals.wonRounds)} won`,
                ].join(' · ')}
              </p>
              <div className="overflow-x-auto rounded-md border-[0.5px] border-border-subtle bg-background">
                <table className="w-full min-w-[720px] border-collapse">
                  <caption className="sr-only">Strategies by sealed result</caption>
                  <thead>
                    <tr className="border-b border-border-subtle">
                      <th className={cn(headClasses, 'pl-xl text-left')} scope="col">
                        #
                      </th>
                      <th className={cn(headClasses, 'text-left')} scope="col">
                        Strategy
                      </th>
                      <th className={cn(headClasses, 'text-right')} scope="col">
                        Rounds sealed
                      </th>
                      <th className={cn(headClasses, 'text-right')} scope="col">
                        Won
                      </th>
                      <th className={cn(headClasses, 'text-right')} scope="col">
                        Result
                      </th>
                      <th className={cn(headClasses, 'pr-xl text-right')} scope="col">
                        Newest round
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {view.rows.map((row, index) => (
                      <BoardRow
                        explorerUrl={explorerUrl}
                        key={row.deploymentId}
                        rank={index + 1}
                        row={row}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
          <p className="mt-md text-caption text-muted-foreground">
            Read from Arc Testnet at {atUtc(view.asOf)}. Contract{' '}
            <a
              className={linkClasses}
              href={under(explorerUrl, `address/${view.contract}`)}
              rel="noopener noreferrer"
              target="_blank"
            >
              {short(view.contract)}
            </a>
            .
            {view.truncated
              ? ' The contract has more transactions than this page reads, so older rounds are not counted.'
              : null}
          </p>
        </>
      )}
    </main>
  )
}
