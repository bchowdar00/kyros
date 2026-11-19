import { describe, expect, it } from 'vitest'

import { OPEN_TX, RECORD_DEPLOYMENT_ID, recordView, seal, sealTx } from './record.fixture'
import { toRecordLine } from './record.mapper'

const urls = { api: 'http://exchange.test', explorer: 'https://explorer.testnet.arc.io' }

describe('toRecordLine', () => {
  it('is pending while the opening is not on chain, whatever the seals say', () => {
    expect(toRecordLine(recordView(null, [seal(3, true)]), urls)).toEqual({ kind: 'pending' })
  })

  it('links the opening transaction when no round is sealed yet', () => {
    expect(toRecordLine(recordView(OPEN_TX, []), urls)).toEqual({
      kind: 'sealed',
      sealedRounds: 0,
      pendingRounds: 0,
      transactionHref: `https://explorer.testnet.arc.io/tx/${OPEN_TX}`,
      journalHref: undefined,
    })
  })

  it('counts sealed and pending rounds and links the highest sealed round', () => {
    const record = recordView(OPEN_TX, [seal(9, false), seal(7, true), seal(8, true)])

    expect(toRecordLine(record, urls)).toEqual({
      kind: 'sealed',
      sealedRounds: 2,
      pendingRounds: 1,
      transactionHref: `https://explorer.testnet.arc.io/tx/${sealTx(8)}`,
      journalHref: `http://exchange.test/api/records/${RECORD_DEPLOYMENT_ID}/8/journal`,
    })
  })

  it('does not double the slash when a base ends with one', () => {
    const line = toRecordLine(recordView(OPEN_TX, [seal(4, true)]), {
      api: 'http://exchange.test/',
      explorer: 'https://explorer.testnet.arc.io/',
    })

    expect(line).toMatchObject({
      transactionHref: `https://explorer.testnet.arc.io/tx/${sealTx(4)}`,
      journalHref: `http://exchange.test/api/records/${RECORD_DEPLOYMENT_ID}/4/journal`,
    })
  })
})
