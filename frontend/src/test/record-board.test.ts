import { describe, expect, it } from 'vitest'

import { decodeRecordLog, type ExplorerLog, type RecordEvent } from '@/features/records/arc-logs'
import { buildRecordBoard } from '@/features/records/board'

// Three logs of the deployed StrategyRecord contract, exactly as Arc Testnet's explorer lists them.
const openedLog: ExplorerLog = {
  block_number: 73_154_349,
  block_timestamp: '2026-11-20T07:44:22.000000Z',
  data: '0x47e34faf00f9626c36ac89b910ea2abe69b11d4f7e967e973003af85b00859270000000000000000000000000000000000000000000000000000000000000fa0',
  topics: [
    '0x10626a129d7628f6a4cb6540632d282bc9a25250656779d609ff70b0545d66cf',
    '0x1e6bdb232f86bd2fc827ebe4e1b13f63c6cf95738896e81b28e5cf412d4942ca',
    null,
    null,
  ],
  transaction_hash: '0x3730572cd3b8110f20a960718eb6a34c46e4bec9abc218e677df51c1fd226e39',
}

const wonLog: ExplorerLog = {
  block_number: 73_159_193,
  block_timestamp: '2026-11-20T08:25:23.000000Z',
  data: '0x0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000001cdc63ba620f1d37bd3a4f0c9cc2960ee90b2d4cdd300c2c399a9c0a2a3d7a025e',
  topics: [
    '0xf849e84085a69739eeb3b0f36b4f9ad8d4548d536aceecaf711fc3f540251c6f',
    '0x1e6bdb232f86bd2fc827ebe4e1b13f63c6cf95738896e81b28e5cf412d4942ca',
    '0x00000000000000000000000000000000000000000000000000000000000000e7',
    null,
  ],
  transaction_hash: '0x0cbbd61fd4b17dfae310dff70510bece043b0eebf3debcebed0fc23684f6d73c',
}

const lostLog: ExplorerLog = {
  block_number: 73_158_586,
  block_timestamp: '2026-11-20T08:20:15.000000Z',
  data: '0x0000000000000000000000000000000000000000000000000000000000000000fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffc1c50597a086621de59d7cf93a4bd7e9a12eac07f06aa7673072f5c7aff10968039',
  topics: [
    '0xf849e84085a69739eeb3b0f36b4f9ad8d4548d536aceecaf711fc3f540251c6f',
    '0x478f2c648121be3a1ca5bbe0a0bfeadcb1f268c686e7ec021e1b5430e6339d12',
    '0x00000000000000000000000000000000000000000000000000000000000000e5',
    null,
  ],
  transaction_hash: '0x092510b124c607381bf63cf34ff000aa3cd0e17eac8fe9d8b2715d0704e40b67',
}

describe('decodeRecordLog', () => {
  it('reads an opening: the deployment, its program hash and its budget in cents', () => {
    expect(decodeRecordLog(openedLog)).toEqual({
      at: '2026-11-20T07:44:22.000000Z',
      budgetCents: 4000,
      deploymentId: '0x1e6bdb232f86bd2fc827ebe4e1b13f63c6cf95738896e81b28e5cf412d4942ca',
      kind: 'opened',
      programHash: '0x47e34faf00f9626c36ac89b910ea2abe69b11d4f7e967e973003af85b0085927',
      transactionHash: '0x3730572cd3b8110f20a960718eb6a34c46e4bec9abc218e677df51c1fd226e39',
    })
  })

  it('reads a sealed round that won', () => {
    expect(decodeRecordLog(wonLog)).toEqual({
      at: '2026-11-20T08:25:23.000000Z',
      deploymentId: '0x1e6bdb232f86bd2fc827ebe4e1b13f63c6cf95738896e81b28e5cf412d4942ca',
      journalHash: '0xdc63ba620f1d37bd3a4f0c9cc2960ee90b2d4cdd300c2c399a9c0a2a3d7a025e',
      kind: 'sealed',
      pnlCents: 28,
      roundId: 231,
      transactionHash: '0x0cbbd61fd4b17dfae310dff70510bece043b0eebf3debcebed0fc23684f6d73c',
      won: true,
    })
  })

  it('reads a loss as a negative number of cents', () => {
    expect(decodeRecordLog(lostLog)).toMatchObject({
      kind: 'sealed',
      pnlCents: -996,
      roundId: 229,
      won: false,
    })
  })

  it('reads a log whose hex is in capitals', () => {
    const upper = {
      ...wonLog,
      data: `0x${wonLog.data.slice(2).toUpperCase()}`,
      topics: wonLog.topics.map(topic =>
        topic === null ? null : `0x${topic.slice(2).toUpperCase()}`
      ),
    }

    expect(decodeRecordLog(upper)).toMatchObject({
      deploymentId: '0x1e6bdb232f86bd2fc827ebe4e1b13f63c6cf95738896e81b28e5cf412d4942ca',
      kind: 'sealed',
      pnlCents: 28,
      roundId: 231,
    })
  })

  it('ignores a log of another event', () => {
    const other = { ...wonLog, topics: [`0x${'ab'.repeat(32)}`, wonLog.topics[1] ?? null] }

    expect(decodeRecordLog(other)).toBeUndefined()
  })

  it('ignores a log whose data is cut short or is not hex', () => {
    expect(decodeRecordLog({ ...wonLog, data: wonLog.data.slice(0, 100) })).toBeUndefined()
    expect(decodeRecordLog({ ...openedLog, data: `0x${'zz'.repeat(64)}` })).toBeUndefined()
  })

  it('ignores a sealed round with no round topic', () => {
    expect(decodeRecordLog({ ...wonLog, topics: wonLog.topics.slice(0, 2) })).toBeUndefined()
  })
})

function sealed(
  deploymentId: string,
  roundId: number,
  pnlCents: number,
  at = '2026-11-20T08:00:00.000000Z'
): RecordEvent {
  return {
    at,
    deploymentId,
    journalHash: `0x${'11'.repeat(32)}`,
    kind: 'sealed',
    pnlCents,
    roundId,
    transactionHash: `0x${roundId.toString(16).padStart(64, '0')}`,
    won: pnlCents > 0,
  }
}

function opened(deploymentId: string, budgetCents: number): RecordEvent {
  return {
    at: '2026-11-20T07:00:00.000000Z',
    budgetCents,
    deploymentId,
    kind: 'opened',
    programHash: `0x${'22'.repeat(32)}`,
    transactionHash: `0x${'33'.repeat(32)}`,
  }
}

const A = `0x${'a'.repeat(64)}`
const B = `0x${'b'.repeat(64)}`
const C = `0x${'c'.repeat(64)}`

describe('buildRecordBoard', () => {
  it('adds up each strategy: rounds, wins, result, and its newest sealed round', () => {
    const board = buildRecordBoard([
      sealed(A, 7, 120, '2026-11-20T08:10:00.000000Z'),
      opened(A, 4000),
      sealed(A, 9, -50, '2026-11-20T08:20:00.000000Z'),
      sealed(A, 8, 0, '2026-11-20T08:15:00.000000Z'),
    ])

    expect(board.rows).toEqual([
      {
        budgetCents: 4000,
        deploymentId: A,
        lastRoundId: 9,
        lastSealedAt: '2026-11-20T08:20:00.000000Z',
        lastTransactionHash: `0x${'9'.padStart(64, '0')}`,
        openTransactionHash: `0x${'33'.repeat(32)}`,
        pnlCents: 70,
        rounds: 3,
        wonRounds: 1,
      },
    ])
    expect(board.totals).toEqual({ rounds: 3, strategies: 1, wonRounds: 1 })
  })

  it('lists the newest sealed rounds of any strategy, newest first, each round once', () => {
    const board = buildRecordBoard([
      sealed(A, 7, 120, '2026-11-20T08:10:00.000000Z'),
      sealed(B, 3, -40, '2026-11-20T08:30:00.000000Z'),
      opened(A, 4000),
      sealed(A, 9, 0, '2026-11-20T08:20:00.000000Z'),
      // The explorer may list a log twice.
      sealed(B, 3, -40, '2026-11-20T08:30:00.000000Z'),
    ])

    expect(board.recent).toEqual([
      {
        at: '2026-11-20T08:30:00.000000Z',
        deploymentId: B,
        pnlCents: -40,
        roundId: 3,
        transactionHash: `0x${'3'.padStart(64, '0')}`,
        won: false,
      },
      {
        at: '2026-11-20T08:20:00.000000Z',
        deploymentId: A,
        pnlCents: 0,
        roundId: 9,
        transactionHash: `0x${'9'.padStart(64, '0')}`,
        won: false,
      },
      {
        at: '2026-11-20T08:10:00.000000Z',
        deploymentId: A,
        pnlCents: 120,
        roundId: 7,
        transactionHash: `0x${'7'.padStart(64, '0')}`,
        won: true,
      },
    ])
  })

  it('keeps only the twelve newest sealed rounds in that list', () => {
    const events = Array.from({ length: 15 }, (_, index) =>
      sealed(A, index + 1, 10, `2026-11-20T08:${String(index + 10)}:00.000000Z`)
    )

    const { recent } = buildRecordBoard(events)

    expect(recent).toHaveLength(12)
    expect(recent[0]?.roundId).toBe(15)
    expect(recent.at(-1)?.roundId).toBe(4)
  })

  it('ranks by result, then by rounds sealed, then by id', () => {
    const board = buildRecordBoard([
      sealed(C, 1, 10),
      sealed(B, 1, 5),
      sealed(B, 2, 5),
      sealed(A, 1, 10),
      sealed(`0x${'d'.repeat(64)}`, 1, 300),
    ])

    expect(board.rows.map(row => row.deploymentId)).toEqual([`0x${'d'.repeat(64)}`, B, A, C])
  })

  it('leaves out a strategy that was opened and never sealed a round', () => {
    const board = buildRecordBoard([opened(A, 4000), opened(B, 10_000), sealed(B, 3, -20)])

    expect(board.rows.map(row => row.deploymentId)).toEqual([B])
    expect(board.totals.strategies).toBe(1)
  })

  it('counts a round once when the explorer lists its log twice', () => {
    const board = buildRecordBoard([sealed(A, 4, 25), sealed(A, 4, 25)])

    expect(board.rows[0]).toMatchObject({ pnlCents: 25, rounds: 1, wonRounds: 1 })
  })

  it('keeps a strategy whose opening is older than the logs read, with no budget', () => {
    const board = buildRecordBoard([sealed(A, 4, 25)])

    expect(board.rows[0]?.budgetCents).toBeUndefined()
    expect(board.rows[0]?.openTransactionHash).toBeUndefined()
  })

  it('takes the newest round by block time, whatever the round ids are', () => {
    const board = buildRecordBoard([
      sealed(A, 900, 5, '2026-11-20T03:00:00.000000Z'),
      sealed(A, 120, 5, '2026-11-20T08:00:00.000000Z'),
    ])

    expect(board.rows[0]).toMatchObject({
      lastRoundId: 120,
      lastSealedAt: '2026-11-20T08:00:00.000000Z',
      lastTransactionHash: `0x${(120).toString(16).padStart(64, '0')}`,
    })
  })
})
