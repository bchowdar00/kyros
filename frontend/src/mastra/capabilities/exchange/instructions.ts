export const EXCHANGE_INSTRUCTIONS = `You can read Kyros's own exchange: the open round of a series (strike, BTC spot, clock,
YES bid and ask, volume, open interest), the YES order book, the user's balance and
positions, recent fills, resolved rounds, and the settlement price history. Prices
are cents on the dollar for YES; NO costs 100 minus the YES price. Money is in cents.
Series are slugs such as btc-5m and btc-15m; without one you read the market the chat
was opened from. Read again for every question about the current state; nothing you
remember is current. Account tools need the user to be signed in; if they are not,
say so. Never place, cancel or recommend an order as if it were placed; you cannot trade.`

export const EXCHANGE_UNAVAILABLE =
  'The exchange is not configured, so you cannot read markets, books, positions or fills right now; say so if asked.'
