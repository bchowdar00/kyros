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

/** One sealed round, as its transaction on Arc states it. */
export interface RecentSeal {
  /** Block time of the transaction (ISO 8601, UTC). */
  at: string
  deploymentId: string
  pnlCents: number
  roundId: number
  transactionHash: string
  won: boolean
}

/** How many of the newest sealed rounds the board lists. */
const RECENT_SEALS = 12

export interface RecordBoard {
  /** The newest sealed rounds of any strategy, newest first. */
  recent: RecentSeal[]
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
  const sealedRounds: SealedEvent[] = []
  for (const [deploymentId, rounds] of seals) {
    sealedRounds.push(...rounds.values())
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

  const recent = sealedRounds
    // Newest by block time; the rest only keeps the order from depending on how the logs came.
    .toSorted(
      (a, b) =>
        (a.at < b.at ? 1 : a.at > b.at ? -1 : 0) ||
        b.roundId - a.roundId ||
        (a.deploymentId < b.deploymentId ? -1 : a.deploymentId > b.deploymentId ? 1 : 0)
    )
    .slice(0, RECENT_SEALS)
    .map(
      (seal): RecentSeal => ({
        at: seal.at,
        deploymentId: seal.deploymentId,
        pnlCents: seal.pnlCents,
        roundId: seal.roundId,
        transactionHash: seal.transactionHash,
        won: seal.won,
      })
    )

  return {
    recent,
    rows,
    totals: {
      rounds: rows.reduce((sum, row) => sum + row.rounds, 0),
      strategies: rows.length,
      wonRounds: rows.reduce((sum, row) => sum + row.wonRounds, 0),
    },
  }
}
