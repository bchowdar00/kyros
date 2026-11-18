Messages and responses exactly as Gemini's public prediction-market API sent them, captured on 2026-11-18 with no account and no key.

- `depth.jsonl`: every `depthUpdate` frame for GEMI-BTC05M2611181720-UP over 75 seconds, from a connection opened with `?snapshot=-1` (the first frame is the full book).
- `book_after_depth.json`: `GET /v1/book/GEMI-BTC05M2611181720-UP` read just after the last frame; the frames must rebuild to this book.
- `book_ticker.jsonl`, `trade.jsonl`, `index.jsonl`, `contract_status.jsonl`, `ack.jsonl`: one or two messages of each other kind.
- `events_btc.json`: `GET /v1/prediction-markets/events?status=active&category=crypto&limit=100`, cut down to the BTC05M and BTC15M events (each event object unchanged).
