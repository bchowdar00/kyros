'use client'

import { env } from '@/env'

import { toRecordLine } from './record.mapper'
import { useDeploymentRecord } from './use-deployment-record.query'

const focusRingClasses =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'
const linkClasses = `rounded-xs text-foreground transition-colors hover:text-muted-foreground ${focusRingClasses}`
const lineClasses = 'flex min-w-0 items-center gap-sm truncate text-caption text-muted-foreground'

export interface RecordLineProps {
  /** Exchange origin that serves the record and the round journals. */
  apiUrl: string
  /** The strategy's sub-account address. */
  address: string
}

function rounds(count: number): string {
  return `${count} ${count === 1 ? 'round' : 'rounds'}`
}

/** Whether this strategy's record is on chain (Arc Testnet), with links to check it there. */
export function RecordLine({ apiUrl, address }: RecordLineProps) {
  const { data, isError } = useDeploymentRecord(apiUrl, address)

  if (data === undefined) {
    return isError ? <p className={lineClasses}>Record unavailable</p> : null
  }
  if (data.status === 'none') return null

  const line = toRecordLine(data.record, { api: apiUrl, explorer: env.NEXT_PUBLIC_EXPLORER_URL })
  if (line.kind === 'pending') return <p className={lineClasses}>Record pending</p>

  const pending = line.pendingRounds > 0 ? ` · ${line.pendingRounds} pending` : ''

  return (
    <p className={lineClasses}>
      <span className="truncate">{`Record sealed · ${rounds(line.sealedRounds)}${pending}`}</span>
      <a
        className={linkClasses}
        href={line.transactionHref}
        rel="noopener noreferrer"
        target="_blank"
      >
        View on Arc<span className="sr-only">: this strategy's record on the Arc explorer</span>
      </a>
      {line.journalHref === undefined ? null : (
        <a
          className={linkClasses}
          href={line.journalHref}
          rel="noopener noreferrer"
          target="_blank"
        >
          Journal<span className="sr-only">: the latest sealed round, line by line</span>
        </a>
      )}
    </p>
  )
}
