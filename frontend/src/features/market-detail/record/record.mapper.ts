import type { RustRecordView } from '@kyros/domain'

type RustSealView = RustRecordView['seals'][number]

export type RecordLine =
  | { kind: 'pending' }
  | {
      kind: 'sealed'
      sealedRounds: number
      pendingRounds: number
      transactionHref: string
      journalHref: string | undefined
    }

/** `path` under `base`, keeping any path the base has and never doubling its slash. */
function under(base: string, path: string): string {
  return new URL(path, base.endsWith('/') ? base : `${base}/`).href
}

/**
 * One deployment's on-chain record as the row's line. Nothing counts as sealed until the
 * opening is on chain; after that the link goes to the newest sealed round (the seals arrive
 * in no particular order), or to the opening while no round is sealed yet.
 */
export function toRecordLine(
  record: RustRecordView,
  urls: { api: string; explorer: string }
): RecordLine {
  if (record.open_tx === null) return { kind: 'pending' }

  let latest: (RustSealView & { tx_hash: string }) | undefined
  let sealedRounds = 0
  for (const seal of record.seals) {
    if (seal.tx_hash === null) continue
    sealedRounds += 1
    if (latest === undefined || seal.round_id > latest.round_id) {
      latest = { ...seal, tx_hash: seal.tx_hash }
    }
  }

  return {
    kind: 'sealed',
    sealedRounds,
    pendingRounds: record.seals.length - sealedRounds,
    transactionHref: under(urls.explorer, `tx/${latest?.tx_hash ?? record.open_tx}`),
    journalHref:
      latest === undefined
        ? undefined
        : under(urls.api, `api/records/${record.deployment_id}/${latest.round_id}/journal`),
  }
}
