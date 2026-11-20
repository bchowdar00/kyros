import * as z from 'zod/mini'

/**
 * The two events of the `StrategyRecord` contract (`chain/contracts/StrategyRecord.sol`), by
 * the hash of their signature:
 * `Opened(bytes32 indexed deploymentId, bytes32 programHash, uint64 budgetCents)` and
 * `Sealed(bytes32 indexed deploymentId, uint64 indexed roundId, bool won, int64 pnlCents, bytes32 journalHash)`.
 */
const OPENED_TOPIC = '0x10626a129d7628f6a4cb6540632d282bc9a25250656779d609ff70b0545d66cf'
const SEALED_TOPIC = '0xf849e84085a69739eeb3b0f36b4f9ad8d4548d536aceecaf711fc3f540251c6f'

/** One log as the Arc explorer's address-logs list returns it. Only the fields read here. */
export const ExplorerLogSchema = z.object({
  block_number: z.number(),
  block_timestamp: z.string(),
  data: z.string(),
  topics: z.array(z.nullable(z.string())),
  transaction_hash: z.string(),
})

export type ExplorerLog = z.infer<typeof ExplorerLogSchema>

export type RecordEvent =
  | {
      kind: 'opened'
      /** Block time of the transaction, as the explorer gives it (ISO 8601, UTC). */
      at: string
      budgetCents: number
      deploymentId: string
      programHash: string
      transactionHash: string
    }
  | {
      kind: 'sealed'
      at: string
      deploymentId: string
      journalHash: string
      /** Paper result of the round in whole cents; negative is a loss. */
      pnlCents: number
      roundId: number
      transactionHash: string
      won: boolean
    }

const WORD = 64
const wordPattern = /^[0-9a-f]{64}$/

/** The 32-byte words of an ABI-encoded value, or nothing if it is not exactly `count` of them. */
function words(hex: string, count: number): string[] | undefined {
  const body = hex.startsWith('0x') ? hex.slice(2).toLowerCase() : undefined
  if (body === undefined || body.length !== count * WORD) return undefined

  const out: string[] = []
  for (let index = 0; index < count; index += 1) {
    const word = body.slice(index * WORD, (index + 1) * WORD)
    if (!wordPattern.test(word)) return undefined
    out.push(word)
  }
  return out
}

/** An unsigned word as a number, when it is small enough to be one exactly. */
function unsigned(word: string): number | undefined {
  const value = BigInt(`0x${word}`)
  return value <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(value) : undefined
}

/** A word holding a sign-extended int64. */
function signed64(word: string): number | undefined {
  const value = BigInt.asIntN(64, BigInt(`0x${word}`))
  const fits = value <= BigInt(Number.MAX_SAFE_INTEGER) && value >= BigInt(Number.MIN_SAFE_INTEGER)
  return fits ? Number(value) : undefined
}

/**
 * One contract log as a record event. A log of any other event, or one whose topics or data
 * do not have the event's shape, is nothing: the board is built only from what decodes.
 */
export function decodeRecordLog(log: ExplorerLog): RecordEvent | undefined {
  const [signatureTopic, deploymentTopic, roundTopic] = log.topics
  const signature = signatureTopic?.toLowerCase()
  const [deploymentId] = words(deploymentTopic ?? '', 1) ?? []
  if (deploymentId === undefined) return undefined

  const base = {
    at: log.block_timestamp,
    deploymentId: `0x${deploymentId}`,
    transactionHash: log.transaction_hash,
  }

  if (signature === OPENED_TOPIC) {
    const [programHash, budget] = words(log.data, 2) ?? []
    const budgetCents = budget === undefined ? undefined : unsigned(budget)
    if (programHash === undefined || budgetCents === undefined) return undefined

    return { ...base, budgetCents, kind: 'opened', programHash: `0x${programHash}` }
  }

  if (signature === SEALED_TOPIC) {
    const [round] = words(roundTopic ?? '', 1) ?? []
    const [won, pnl, journalHash] = words(log.data, 3) ?? []
    const roundId = round === undefined ? undefined : unsigned(round)
    const pnlCents = pnl === undefined ? undefined : signed64(pnl)
    if (
      roundId === undefined ||
      pnlCents === undefined ||
      won === undefined ||
      journalHash === undefined
    ) {
      return undefined
    }

    return {
      ...base,
      journalHash: `0x${journalHash}`,
      kind: 'sealed',
      pnlCents,
      roundId,
      won: BigInt(`0x${won}`) !== BigInt(0),
    }
  }

  return undefined
}
