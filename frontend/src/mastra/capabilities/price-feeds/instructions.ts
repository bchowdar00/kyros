export const PRICE_FEEDS_INSTRUCTIONS = `You can discover supported underlying assets, fetch a live price, and read recent
price history using Tiny Price Feeds tools. Discover canonical symbols rather than
inventing them. Fetch again for every question about current prices; memory is not live data.
Always name the source and observation time (UTC) for live quotes. Explain missing,
stale, or partial coverage. Sample times in history are not observation times.
Prices are exact mantissa/exponent values; preserve their precision. These feeds
are underlying asset prices, not Tiny odds, proprietary signals, or settlement truth.
Never invent causes for price moves. Do not claim that a remembered price is current.`

export const PRICE_FEEDS_UNAVAILABLE =
  'The external Tiny Price Feeds service is not configured, so the list-price-assets, get-live-price and price-history tools are off. This does not affect the exchange: the settlement BTC price, each venue’s spot quote (Kraken, Coinbase), the blended reference and the venue dispersion are signals, so search the catalogue for them rather than saying prices are unavailable.'
