import type { RecordEvent } from './arc-logs'

/** One strategy's sealed record, added up from its transactions on Arc. */
export interface RecordBoardRow {
  /** The budget it was opened with; unknown when its opening is older than the logs read. */
  budgetCents: number | undefined
  /** The id the contract keys the record by. */
  deploymentId: string
  lastRoundId: number
  lastSealedAt: string
  lastTransactionHash: string
  openTransactionHash: string | undefined
  /** Paper result over its sealed rounds, in cents. */
  pnlCents: number
  rounds: number
  wonRounds: number
}

export interface RecordBoard {
  rows: RecordBoardRow[]
  totals: { rounds: number; strategies: number; wonRounds: number }
}

type SealedEvent = Extract<RecordEvent, { kind: 'sealed' }>
type OpenedEvent = Extract<RecordEvent, { kind: 'opened' }>

/**
 * The board: every strategy with at least one sealed round, best result first (then more
 * rounds, then id, so the order never depends on the order the logs arrived in). A round is
 * counted once however many times its log is listed; the contract seals a round only once.
 */
export function buildRecordBoard(events: readonly RecordEvent[]): RecordBoard {
  const openings = new Map<string, OpenedEvent>()
  const seals = new Map<string, Map<number, SealedEvent>>()

  for (const event of events) {
    if (event.kind === 'opened') {
      openings.set(event.deploymentId, event)
      continue
    }
    const rounds = seals.get(event.deploymentId) ?? new Map<number, SealedEvent>()
    if (!rounds.has(event.roundId)) rounds.set(event.roundId, event)
    seals.set(event.deploymentId, rounds)
  }

  const rows: RecordBoardRow[] = []
  for (const [deploymentId, rounds] of seals) {
    let last: SealedEvent | undefined
    let pnlCents = 0
    let wonRounds = 0
    for (const seal of rounds.values()) {
      pnlCents += seal.pnlCents
      if (seal.won) wonRounds += 1
      // Newest by block time: round ids are only known to rise within some series.
      const newer =
        last === undefined ||
        seal.at > last.at ||
        (seal.at === last.at && seal.roundId > last.roundId)
      if (newer) last = seal
    }
    if (last === undefined) continue

    const opening = openings.get(deploymentId)
    rows.push({
      budgetCents: opening?.budgetCents,
      deploymentId,
      lastRoundId: last.roundId,
      lastSealedAt: last.at,
      lastTransactionHash: last.transactionHash,
      openTransactionHash: opening?.transactionHash,
      pnlCents,
      rounds: rounds.size,
      wonRounds,
    })
  }

  rows.sort(
    (a, b) =>
      b.pnlCents - a.pnlCents ||
      b.rounds - a.rounds ||
      (a.deploymentId < b.deploymentId ? -1 : a.deploymentId > b.deploymentId ? 1 : 0)
  )

  return {
    rows,
    totals: {
      rounds: rows.reduce((sum, row) => sum + row.rounds, 0),
      strategies: rows.length,
      wonRounds: rows.reduce((sum, row) => sum + row.wonRounds, 0),
    },
  }
}
