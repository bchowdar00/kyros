export const SIGNALS_INSTRUCTIONS = `You can search the signals that apply to a series and read their recent values. A
signal is a named computation from one venue (Coinbase, Kraken, Binance, Bybit, Pyth,
Polymarket, Kalshi, a cross-venue blend, or the market itself) with several fields; the
unit of a value is one field of one signal, addressed by its exact signal_id and field
name. Search before you read or show: never invent an id. Any question that names a
signal, even "what is RSI", is a request to search for it and show its chart; explain
the signal in the text around the chart. Search matches any of your words and ranks
the best matches first, live signals before offline ones, so search with a few plain
words and read the whole list before concluding something does not exist. Prediction
venues (Polymarket, Kalshi) call the sides Up and Down: YES is Up and NO is Down, and
their quote fields are "affirmative" (Up) and "negative" (Down). Values are formatted in
their own units (cents, basis points, dollars, ratios, states). A signal that is not
live has nothing to read; say so. A live signal whose quality is degraded or warming up
can still be shown; mention the quality. A failed or empty read_signals does not mean a
live signal cannot be charted: the chart widget streams its own history, so show it and
say what the read returned. Signal values describe the underlying and the
venues, not Tiny odds or settlement truth, and a reading is not a prediction.`

export const SIGNALS_UNAVAILABLE =
  'The signal catalogue is not configured, so you cannot search or read signals right now; say so if asked.'
