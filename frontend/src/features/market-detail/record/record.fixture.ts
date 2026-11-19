import type { RustRecordView } from '@kyros/domain'

type RustSealView = RustRecordView['seals'][number]

export const RECORD_ACCOUNT = `0x${'ab'.repeat(20)}`
export const RECORD_DEPLOYMENT_ID = `0x${'d1'.repeat(32)}`
export const OPEN_TX = `0x${'0e'.repeat(32)}`

/** A deterministic 32-byte transaction hash per round. */
export function sealTx(roundId: number): string {
  return `0x${roundId.toString(16).padStart(2, '0').repeat(32)}`
}

export function seal(roundId: number, onChain: boolean): RustSealView {
  return {
    round_id: roundId,
    won: roundId % 2 === 0,
    pnl_cents: 125,
    journal_hash: `0x${'4a'.repeat(32)}`,
    tx_hash: onChain ? sealTx(roundId) : null,
    lines: 12,
  }
}

export function recordView(openTx: string | null, seals: readonly RustSealView[]): RustRecordView {
  return {
    deployment_id: RECORD_DEPLOYMENT_ID,
    account: RECORD_ACCOUNT,
    series: 'btc-15m',
    deployment: 'late-closer',
    strategy_id: 'late-closer',
    program_hash: `0x${'9f'.repeat(32)}`,
    budget_cents: 5_000,
    open_tx: openTx,
    seals: [...seals],
  }
}
