import { expect, it } from 'vitest'
import { applyAccount, initialAccount } from './account'
it('takes only newer balances and clears the baseline on reconnect', () => {
  const live = applyAccount(initialAccount, {
    type: 'balance',
    body: { balance_seq: '9007199254740994', available: '10', locked: '5', pending_finality: '0' },
  })
  const old = applyAccount(live, {
    type: 'balance',
    body: { balance_seq: '9007199254740993', available: '1', locked: '0', pending_finality: '0' },
  })
  expect(old.available).toBe('10')
  expect(applyAccount(old, { type: 'snapshot_begin', body: {} }).available).toBeNull()
})
it('removes a filled order and applies absolute positions', () => {
  const accepted = applyAccount(initialAccount, {
    type: 'order_accepted',
    body: { order_id: '5', market_id: `${'0'.repeat(63)}7`, price: 501, side: 'buy', qty: '2' },
  })
  const filled = applyAccount(accepted, {
    type: 'fill',
    body: {
      fee: '0',
      book_seq: '3',
      fill_index: 0,
      order_id: '5',
      market_id: `${'0'.repeat(63)}7`,
      qty: '2',
      price: 501,
      remaining: '0',
      position_yes: '2',
      position_no: '0',
    },
  })
  expect(filled.orders).toEqual([])
  expect(filled.positions[0]?.yes).toBe('2')
})
