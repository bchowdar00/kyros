import { describe, expect, it } from 'vitest'

import { cancelFailureNotice, deployFailureNotice, orderFailureNotice } from './order-error.mapper'

const UNCONFIRMED =
  'The exchange did not confirm this order. Check Open Orders and Trade History before trying again.'
const refused = (message: string) => ({ message, status: 422 })
const unanswered = (message: string) => ({ message, status: undefined })

describe('orderFailureNotice', () => {
  it.each([
    ['market_closed', 'This round is closed to new orders. The next one opens shortly.'],
    ['no_liquidity', "Nothing is offered at that price on Gemini's book right now."],
    ['insufficient_balance', 'Not enough paper balance for this order.'],
    ['zero_quantity', 'That amount buys less than one contract.'],
    ['bad_price', "That price is not on this contract's price grid."],
  ])('reads a %s refusal as not placed, with its copy', (code, copy) => {
    expect(orderFailureNotice(refused(`${code}: server wording`))).toBe(`Order not placed. ${copy}`)
  })

  it("passes an unknown code's own text through as a sentence", () => {
    expect(orderFailureNotice(refused("wrong_series: series btc-5m is not this account's"))).toBe(
      "Order not placed. Series btc-5m is not this account's."
    )
  })

  it('says only that the order was refused when the code carries no text', () => {
    expect(orderFailureNotice(refused('wrong_series: '))).toBe(
      'Order not placed. The order was refused.'
    )
  })

  it.each([
    'Exchange unreachable',
    'Exchange did not answer in time',
    'Exchange response is not JSON',
    'Exchange response did not match its contract',
    '',
    ': no code',
  ])('never claims "%s" means the order was not placed', message => {
    expect(orderFailureNotice(unanswered(message))).toBe(UNCONFIRMED)
  })

  it('never claims a bare HTTP status means the order was not placed', () => {
    expect(orderFailureNotice({ message: '503 Service Unavailable', status: 503 })).toBe(
      UNCONFIRMED
    )
  })

  it.each([500, 502, 503])(
    'does not take a worded %i answer as proof the order was not placed',
    status => {
      expect(orderFailureNotice({ message: 'internal: boom', status })).toBe(UNCONFIRMED)
    }
  )

  it('does not take refusal wording without an HTTP answer as a refusal', () => {
    expect(orderFailureNotice(unanswered('market_closed: market 7 is closed'))).toBe(UNCONFIRMED)
  })
})

describe('deployFailureNotice', () => {
  it("reads a refusal as not deployed, in the exchange's words", () => {
    expect(deployFailureNotice(refused('insufficient_balance: insufficient balance'))).toBe(
      'Not deployed. Insufficient balance.'
    )
  })

  it.each([
    unanswered('Exchange did not answer in time'),
    unanswered('Exchange unreachable'),
    { message: 'internal: boom', status: 500 },
  ])('never claims an unconfirmed deployment was refused', failure => {
    expect(deployFailureNotice(failure)).toBe(
      'The exchange did not confirm this deployment. Check My Accounts before trying again.'
    )
  })
})

describe('cancelFailureNotice', () => {
  it("reads a refusal as a failed cancel, in the exchange's words", () => {
    expect(cancelFailureNotice({ message: 'unknown_order: order 9 not found', status: 404 })).toBe(
      'Cancel failed. Order 9 not found.'
    )
  })

  it('never claims an unconfirmed cancel failed', () => {
    expect(cancelFailureNotice(unanswered('Exchange did not answer in time'))).toBe(
      'The exchange did not confirm the cancel. Check Open Orders before trying again.'
    )
  })
})
