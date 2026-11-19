# Kyros

Say what you would do in a market, in one sentence. Kyros turns it into a strategy, checks it, and runs it with pretend money on Gemini's live prediction markets, Bitcoin every five minutes and one NBA game at a time, inside a budget you set. Each round it trades is sealed on Arc once the result is in, so a strategy's record cannot be edited or cherry-picked later.

- **Live site:** https://kyros.happyocean-14c358ad.westus2.azurecontainerapps.io
- **Contract (Arc Testnet, chain id 5042002):** `StrategyRecord` at `0xc910cfe6b537feee16b60a9964c55fc66580586a`, https://explorer.testnet.arc.io/address/0xc910cfe6b537feee16b60a9964c55fc66580586a
- **Entry:** UBC 2026 Hackathon. Track: Consumer Applications. Chain award: Best use of Arc. Bounty: Gemini Prediction Markets.

## What you can do on the site

1. Open it. There is no sign-up and no wallet. You get a paper account with $1,000 of pretend money.
2. Open **BTC Up or Down 5m**. It is Gemini's own five-minute Bitcoin contract: the same strike, the same close, and Gemini's order book, live. The contract's symbol links to it on Gemini.
3. Or open the **NBA game**. When Gemini lists a game, Kyros follows its moneyline: the home team's contract is the book, the score and the last play come from the public scoreboard, and Polymarket's price for the same game is one of the live signals a strategy can read (open **Signals** to see it).
4. Build a strategy by clicking the words of a sentence ("Buy Up when BTC is 10 basis points above the strike with two minutes left"), or write your own. Claude writes the strategy; a compiler checks it before any money moves.
5. Give it a budget and deploy it. It runs in its own account. You can also run a ready-made strategy, or its inverse: seven for Bitcoin, six for the game.
6. When a round your strategy traded in closes, the result is sealed on Arc. The row shows "Record sealed", a link to the transaction, and the journal the hash was taken from.

## What is real and what is pretend

| | |
|---|---|
| Markets, strikes, closing times, results | Gemini's. A round here is one Gemini contract and covers exactly its window. |
| The order book | Gemini's, mirrored live. If a side is empty on Gemini, the page says so. |
| The game's score and plays | ESPN's public scoreboard. If the feed drops, the page says "Score feed disconnected". |
| The other venue's price | Polymarket's, for the same game, read from its public API. A signal only: Kyros sends nothing to Polymarket. |
| The money | Pretend. Paper mode is the only mode on the public site. The badge in the masthead says which mode a server is in. |
| Fills | Simulated. A pretend order fills against Gemini's mirrored book, at the price and size resting there; a resting one also fills when Gemini trades through its price, or against another visitor's pretend order on the same book. No liquidity is invented. |
| Other traders, crowd counts, leaderboards | None. The site shows no invented number. |
| The record on Arc | Real transactions on Arc Testnet. |

The public site places no order on Gemini. A server you run yourself, with your own Gemini key, can send a strategy's orders to Gemini for real; see Real orders below.

## How it works

```
 your sentence ──► Claude writes a strategy ──► compiler checks it ──► deployment
                                                (a small, closed language;      │
                                                 a failed check moves no money) │
                                                                                ▼
 Gemini (public data)                                   Kyros server (Rust)
   contracts, strikes, results ──► round driver ──► one round per Gemini window
   live order book (socket)    ──► book mirror  ──► the paper engine's book
   spot prices, other venues,  ──► signals      ──► the strategy runtime ──► paper orders
   game scores
                                                                                │
                                                     each settled round         ▼
                                                     ──► journal ──► recorder ──► StrategyRecord on Arc
```

- **Rounds.** For each series (`btc-5m`, `btc-15m`) the server follows Gemini's rolling contracts. A round opens at the contract's window start with Gemini's strike and stops taking orders at Gemini's expiry. The result is Gemini's. An audit route (`/api/rounds/audit`) compares every settled round with Gemini's result.
- **The game.** The third series, `nba-h2h`, is one NBA game at a time from Gemini's moneyline list. The round opens when the game is chosen, so it trades before tipoff, and it settles on Gemini's own result for the contract; if Gemini has published none ten minutes after the scoreboard's final, it settles on the final score. Yes is "home wins". When Gemini lists no game, the page says it is waiting.
- **The book mirror.** One account per market copies Gemini's live book into the engine. The same displayed contracts are never taken twice. Pretend orders from different visitors share that book, so they can also fill each other.
- **Strategies.** A strategy is a document in a small language: triggers over signals, orders with limits, and what to do when an order fills or is refused. The compiler checks it against a registry of the signals and instruments that exist. Nothing else can run.
- **The writer.** Claude turns a sentence into that document. Daily caps, counted on the server, limit how many strategies one visitor, one account and the whole site can have written.
- **The runtime.** A deployment wakes only when one of its triggers fires. Its orders go through one four-call interface (place, cancel, cancel and replace, read the account), and each is checked against the deployment's budget before it is sent.
- **Signals.** For Bitcoin: Gemini's settlement index, spot from Coinbase and Kraken, Polymarket's Bitcoin markets, the round's distance from its strike, and book and flow figures. For the game: each play, the score margin, a win probability worked out from the score, the clock and the pregame line, and Polymarket's price for the same game. A strategy can trade the gap between the two venues, or between the model and the crowd.

## How Kyros uses Arc

A shared strategy is only worth copying if its history is honest. Screenshots are not. So every deployment writes two kinds of fact to `StrategyRecord` on Arc Testnet:

- `open(deploymentId, programHash, budgetCents)`, once, when the strategy is deployed: the hash of the exact compiled program and the budget it committed.
- `seal(deploymentId, roundId, won, pnlCents, journalHash)`, once per settled round the strategy placed an order in: whether it won, its profit or loss in cents, and the hash of that round's journal.

Anyone can fetch a deployment's record (`/api/records/{id}`) and each round's journal (`/api/records/{id}/{round}/journal`) from the server, hash the journal, and compare it with the chain. A sealed round cannot be sealed again, and a deployment cannot be reopened with a different program.

The contract has no owner, no upgrade path and no payable function. It stores no market price of any kind: no strike, no spot, no fill price. Fees are paid in USDC, Arc's gas token, by a recorder service that runs beside the server; a visitor never sees a wallet or a fee.

Source: `chain/contracts/StrategyRecord.sol`. Tests: `chain/test/` and `chain/recorder/test/`.

## How Kyros uses Gemini

On the public site, public market data only, no key: the prediction-market events (contracts, strikes, windows, results) for the Bitcoin series and the NBA moneyline over REST, and the live order book over Gemini's public socket. Code: `server/crates/exchange/src/gemini/`.

## Real orders

Off unless the person running the server turns it on. The server sends orders to Gemini only when its host sets all of `KYROS_REAL_ORDERS=1`, `GEMINI_ENV` (`sandbox` or `production`, spelled out), a Gemini API key and secret, and `KYROS_REAL_OPERATOR`, the one account whose strategies may trade. Anything half-set stops the boot. Then:

- Only that operator's strategy deployments send orders. Every other account, and every order placed by hand, stays pretend.
- An order is at most 25 contracts unless `KYROS_REAL_MAX_ORDER` says otherwise, and is still checked against the deployment's budget first.
- It covers the two Bitcoin series. On the NBA series a real-mode order is refused and nothing is sent.

This path has been tested against a stand-in for Gemini only, not yet against Gemini's sandbox or production. Code: `server/crates/exchange/src/gemini/venue.rs` and `server/crates/gemini-private/`.

## Run it yourself, with no key

You need Rust 1.93 (pinned in `server/rust-toolchain.toml`; rustup fetches it), Node 24 and pnpm.

```bash
# 1. The server, on port 8080. It needs no key: it reads Gemini's public data.
cd server
SIGNALS_ENABLED=0 cargo run
```

That is the light way to run it: markets, the live book, paper trading, and the two ready-made Bitcoin strategies that read only the book and the strike. The other five read outside price feeds, which are off here, so their cards stay hidden. The game's six strategies need no feed switch: when Gemini lists a game, the score and Polymarket's price are read directly. To turn the feeds on, run the server like this instead, and give it room: with these settings the feed recorders wrote 0.41 GiB a minute and peaked at 2.7 GiB on disk in a 16-minute run.

```bash
SIGNALS_ENABLED_VENUES=coinbase,kraken,polymarket SIGNALS_ROTATE_SECONDS=180 SIGNALS_KEEP_GENERATIONS=2 cargo run
```

```bash
# 2. The site, on port 3000.
cd frontend
pnpm install --frozen-lockfile
EXCHANGE_API_URL=http://localhost:8080 NEXT_PUBLIC_API_URL=http://localhost:8080 NEXT_PUBLIC_SITE_URL=http://localhost:3000 pnpm dev
```

Open http://localhost:3000. Two things need more:

- **Writing a strategy from a sentence** needs a Claude API key (`ANTHROPIC_API_KEY`) on the site and the same `WRITER_METER_TOKEN` on the site and the server. Without them the writer is off and says so.
- **Sealing on Arc** needs the contract deployed and the recorder running (`chain/README.md`). Without them rows read "Record pending".

## Tests

```bash
cd server && cargo test --workspace     # 2,351 tests pass
cd frontend && pnpm exec vitest run     # the app: 682 pass, 1 known failure (a strategy-authoring test that expects no rate warning)
cd frontend && pnpm --filter "./packages/**" test   # the libraries: 24, 32, 76 and 28 pass
cd chain && npm install && npm test     # 10 contract tests, 33 recorder tests
```

One long run on 2026-11-18, Bitcoin only: 162 minutes, 42 deployments of the 13 ready-made strategies, 998 paper fills, and the result of every one of 44 rounds matched Gemini's. In that run the deployments shared one pretend book, so some of those fills were against each other. The game market has had no run of that kind.

## Layout

```
server/     Rust. crates/exchange (rounds, book mirror, paper engine, strategy runtime, game signals, API),
            crates/strategy-compiler, crates/gemini-private (the order-signing client), and three signal crates.
frontend/   Next.js 16, React 19, TypeScript. The site and the strategy writer.
chain/      Solidity contract, Hardhat tests, and the recorder service.
```

## Limits, plainly

- Paper money on the public site. Real orders need your own server and your own Gemini key, cover Bitcoin only, and have not been sent to Gemini by anyone yet.
- Bitcoin (five-minute and fifteen-minute contracts) and one NBA game at a time. No other coin, no other sport.
- The game's win probability is a simple model of score and time, not a forecast to bet on. A server restart in the middle of a game shortens the chart's line.
- The server keeps accounts in memory. A restart forgets them; the records already on Arc stay.
- Gemini's five-minute books are thin: a side is sometimes empty, and then nothing fills. The site shows that as it is.
- Arc Testnet, not mainnet.
- The site is built for a desktop screen; a phone sees a "desktop for now" page.

## What next

Real orders for any visitor through their own Gemini account, by Gemini's OAuth, in place of a key on a server they run, so a strategy that did well on paper can be given real money with the same budget and the same record. Real orders on the game. Then ETH, SOL and XRP beside Bitcoin, and more than one game a night.
