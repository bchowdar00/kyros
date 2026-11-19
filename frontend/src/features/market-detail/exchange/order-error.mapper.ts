/** What the exchange's refusal codes mean to someone placing an order. */
const CODE_COPY = new Map([
  ['market_closed', 'This round is closed to new orders. The next one opens shortly.'],
  ['no_liquidity', "Nothing is offered at that price on Gemini's book right now."],
  ['insufficient_balance', 'Not enough paper balance for this order.'],
  ['zero_quantity', 'That amount buys less than one contract.'],
  ['bad_price', "That price is not on this contract's price grid."],
])

const CODE_SEPARATOR = ':'
/** The exchange's error codes are snake_case words (`market_closed`). */
const CODE_SHAPE = /^[a-z][a-z0-9_]*$/
const SENTENCE_END = /[.!?]$/
/** Untrusted text is bounded before any regex runs on it. */
const MAX_MESSAGE_LENGTH = 2_000
const FIRST_CLIENT_ERROR = 400
const FIRST_SERVER_ERROR = 500

/** What `@kyros/api` reports of a failed request: its message and the HTTP status, if any. */
export interface ExchangeFailure {
  message: string
  status: number | undefined
}

/**
 * The exchange's own refusal, or undefined when the outcome is not known. Only a 4xx answer
 * the exchange put into words (`@kyros/api` reports it as `<code>: <server message>`) proves
 * the request did nothing. No answer, an unreadable answer, a bare status or any 5xx may hide
 * a request that went through.
 */
function refusal(failure: ExchangeFailure): { code: string; text: string } | undefined {
  const { status } = failure
  if (status === undefined || status < FIRST_CLIENT_ERROR || status >= FIRST_SERVER_ERROR)
    return undefined
  const bounded = failure.message.slice(0, MAX_MESSAGE_LENGTH)
  const separator = bounded.indexOf(CODE_SEPARATOR)
  const code = bounded.slice(0, separator)
  if (separator <= 0 || !CODE_SHAPE.test(code)) return undefined
  return { code, text: bounded.slice(separator + CODE_SEPARATOR.length).trim() }
}

function sentence(text: string, fallback: string): string {
  if (text.length === 0) return fallback
  const capitalised = `${text.charAt(0).toUpperCase()}${text.slice(1)}`
  return SENTENCE_END.test(capitalised) ? capitalised : `${capitalised}.`
}

/** A known code reads as its own copy, an unknown one as the server's text. */
export function orderFailureNotice(failure: ExchangeFailure): string {
  const refused = refusal(failure)
  if (refused === undefined)
    return 'The exchange did not confirm this order. Check Open Orders and Trade History before trying again.'
  const copy = CODE_COPY.get(refused.code) ?? sentence(refused.text, 'The order was refused.')
  return `Order not placed. ${copy}`
}

/** A deployment moves money into a sub-account, so an unconfirmed one is never called refused. */
export function deployFailureNotice(failure: ExchangeFailure): string {
  const refused = refusal(failure)
  if (refused === undefined)
    return 'The exchange did not confirm this deployment. Check My Accounts before trying again.'
  return `Not deployed. ${sentence(refused.text, 'The exchange refused it.')}`
}

export function cancelFailureNotice(failure: ExchangeFailure): string {
  const refused = refusal(failure)
  if (refused === undefined)
    return 'The exchange did not confirm the cancel. Check Open Orders before trying again.'
  return `Cancel failed. ${sentence(refused.text, 'The exchange refused it.')}`
}
