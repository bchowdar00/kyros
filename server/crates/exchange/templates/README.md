# Strategy templates

One JSON file per public strategy. Each file is the catalogue entry the
frontend shows on a strategy card plus the two IR v0.3 documents
(`strategy_compiler::Strategy`) that a Run or Inverse click deploys. Nothing
here is executed as-is: the loader (`src/templates.rs`) embeds these files,
fills the placeholders, compiles every document against the generated registry
at boot, and refuses to start on a diagnostic or a warning. `GET
/api/strategies/templates?series=` serves the cards.

## File shape

| key | meaning |
|---|---|
| `id` | template id; the count key on deployments |
| `title` | card chrome |
| `creator.handle`, `creator.username`, `creator.avatar` | the author shown on the card: the handle, the display name and the portrait filename under the frontend's `assets/market/avatars/` folder (`null` while no portrait is assigned; the card falls back to the handle's initials) |
| `series` | the series slugs this template is listed on |
| `logic`, `inverse_logic` | the prose shown under the Run and Inverse tabs |
| `envelope` | ratios and lot sizes the deploy route turns into a concrete `Envelope` for the user's allocation: `loss_budget_fraction` × allocation, `order_fraction` (dollars per order as a fraction of the allocation, stamped as `{{stake}}`), `max_order_size` (the lot ceiling every dollar-sized order is capped at), `max_open_orders`, `max_position_per_market` |
| `substitutions` | per-series values for the `{{name}}` placeholders inside the documents |
| `primary`, `inverse` | the two IR documents; the inverse is a strategy in its own right from the same family, never a mirror (see the rules) |

## Placeholders

Documents are text-substituted before parsing, so a placeholder may sit inside
an expression string or a param default.

- `{{series}}` — the series slug (`btc-5m`, `btc-15m`). Every instrument and
  exchange-internal signal id is written as `{{series}}.current…`.
- `{{spot}}` — the live BTC price signal, `pyth.btc_usd.spot.v1` with a Pyth
  key and `coinbase.btc_usd.spot.v1` without (the registry's one `external`
  signal). Both expose `last`.
- `{{stake}}` — the file's `envelope.order_fraction`, so an order's dollars are
  `envelope.allocation * {{stake}}`.
- Everything else comes from `substitutions[<series>]`; a shared template must
  declare every placeholder it uses for every series it lists.

## Rules every template follows

- **Both sides, always.** A document never trades only Up or only Down. The
  primary watches both sides and takes whichever fires; the inverse is a
  different strategy from the same family (a different entry, exit or
  temperament), not the primary with the outcome swapped. On a single
  complementary book "Up cheap" *is* "Down rich", so a mirrored inverse would
  be the same trade.
- **Dollar sizing.** Every order is sized in dollars over the price it will
  pay, capped by the lot ceiling:
  `clamp(envelope.allocation * {{stake}} / <price>, 0, envelope.max_order_size)`.
  A flip adds the current position (`platform.position.size`) so the residual
  is deliberate; a take profit that leaves a residual into settlement is fine.
- **Fit the matching engine.** Trigger conditions are shared rows evaluated
  once per publish for every deployment of the template, so the decision goes
  in the row, not in the block: thresholds are literals (a `param` in a row
  makes it private to one deployment), "either side" is one `any` row over the
  two sides, and computed thresholds (fair value) are `expr` rows over signal
  fields. Entries and exits use `fire: if_true` with `rearm: immediate`: the
  trigger's bookkeeping advances even while a leg masks it, so an edge or a
  `fresh_crossing` that happened while masked is spent and never delivers,
  whereas `immediate` fires on the next publish after the leg arms it. The
  block's guard and its `then` stop the repeats (the machine-gun lint holds
  every order block to that). `never` is wrong for anything that must fire
  again in a later leg. Legs are masks: hunting arms both entries, a holding
  leg arms only its own exit. Only something that needs
  memory across publishes (the direction of fair) uses a bare trigger with a
  cooldown and a state slot. Makers throttle requotes with `rearm.cooldown`
  and pull on a `window_clock` timer before the close. Settlement rewinds
  every document to its first leg.
- **Route every fate.** An immediate-or-cancel that finds no size is an
  `expiry`, not a `cancel_ack`; an on table that routes only `cancel_ack`
  leaves the deployment in a holding leg with nothing held for the rest of
  the round. Every taker's on table names `fill`, `partial`, `reject`,
  `cancel_ack` and `expiry`. Entry and exit rows carry a one-second
  `cooldown` on top of `immediate` so a row that stays true while the block's
  guard fails wakes the deployment once a second, not on every publish.
- Trades exactly one series per document; a shared template is stamped once per
  series, so `fair-value-discount` yields four documents.
- Down orders are written in the Down frame: the Down ask is `1 - book.bid`,
  the Down bid is `1 - book.ask`; the venue complements onto the single Up book.
- The round's distance from its strike comes from `<series>.current.delta.v1`
  (`delta_bps`, `spot`, `strike`), published on every spot tick; a strategy
  that prices the round computes fair from it in an `expr` row.
- Only signals live on this deployment are read: exchange internals, the spot
  feed, the blended `btc.external.*` microstructure set, Coinbase and Kraken
  sweeps, and Polymarket's quote, contract and probability momentum. Polymarket
  templates gate on its strike sitting within `strike_band` of ours and are
  inert otherwise.
- Every signal a document names is a readiness gate. The card route reports
  `ready` and `waiting_on` per side from the compiled `subscriptions`, and
  the frontend hides a card while any of them is warming up, degraded or has
  never typed against the registry, because a deployment made then sits in
  its first leg with zero activations and nothing on the wire saying why.
  Keep a side's signal set to what it actually reads: an unused signal in
  `signals` hides the card for nothing.
- Nicknames avoid builtin names (`abs` is a builtin); decimals are canonical
  (`0.1`, never `0.10`).

## Catalogue

| id | series | kind | reads |
|---|---|---|---|
| fair-value-discount | both | maker | book, strike, spot (digital-call fair via `norm_cdf`) |
| momentum-rider | both | taker | external micro_momentum, aggressor_flow |
| late-closer | both | taker | window clock, strike, spot |
| oracle-gap | both | taker | spot vs external spot_reference, strike |
| order-flow-lead | both | taker | external order_flow_imbalance, actionable_depth_imbalance |
| vwap-reversion | both | maker | external vwap_deviation, book |
| sweep-follower | btc-5m | taker (inverse fades) | coinbase/kraken sweep_detection |
| absorption-fade | btc-5m | taker | external absorption_state |
| polymarket-discount | btc-15m | maker | polymarket prediction_quote, market_contract, strike |
| polymarket-momentum | btc-15m | taker (inverse fades) | polymarket probability_momentum, market_contract |
| trend-rider | btc-15m | taker with stop | external horizon_momentum, micro_momentum, book |
| lead-change | nba-h2h | taker, flips a fixed stake on every lead change, locks in Q4 | margin, pbp phase and quarter, book |
| sharp-money | nba-h2h | taker vs the sharp line (inverse fades noise moves) | sharp_line, book, margin |
| fourth-quarter | nba-h2h | taker, late lock-in (inverse: comeback lottery) | pbp quarter and clock, margin, book |
| pregame-pick | nba-h2h | taker before tipoff | sharp_line, window clock, book |
| team-loyalty | nba-h2h | taker before tipoff on one team's games | matchup, window clock, book |
| shooting-streak | nba-h2h | taker on make/miss streaks counted in state | pbp action, result and team, book |

NBA documents guard in-play rows with `pbp.phase == in_play`, because the
play-by-play keeps the previous game's `final` as its latest state through
the next round's pregame; pregame rows use the round clock's `remaining`
above the tipoff mark (a five-minute round tips off after sixty seconds).
The book quotes `home`; `away` prices are `1 - book.bid` and the venue
complements them onto the home book.
