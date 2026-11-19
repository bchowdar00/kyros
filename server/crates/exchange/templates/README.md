# Strategy templates

One JSON file per public strategy. Each file is the catalogue entry the
frontend shows on a strategy card plus the two IR v0.3 documents
(`strategy_compiler::Strategy`) that a Run or Inverse click deploys. Nothing
here is executed as-is: the loader (`src/templates.rs`) embeds these files,
fills the placeholders, compiles every document against the generated registry
at boot, and refuses to start on a diagnostic or a warning. `GET
/api/strategies/templates?series=` serves the cards.

The `parked/` subfolder holds templates that are not embedded or loaded. They
target an `nba-h2h` series that the server no longer runs, so they cannot
compile against the generated registry. Bringing them back takes more than
moving the files: the series row and its `home`/`away` outcomes, the NBA
signals in the registry, their copy files and a round driver all went with
the simulator, and so did the tests of the home/away frame. Commit `e5b269f`
has every one of them.

## File shape

| key | meaning |
|---|---|
| `id` | template id; the count key on deployments |
| `title` | card chrome |
| `listed` | optional boolean, default `true` when absent; `listed: false` takes the template off the public list: it is still compiled, loadable by id and deployable, but the list route (`GET /api/strategies/templates?series=`) leaves it out |
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
- `{{spot}}` — the round's reference price signal, `gemini.btc_usd.spot.v1`
  (Gemini's index price, the one its contracts settle on). It exposes `last`.
  No listed document uses it today: they read the distance from the strike
  through `<series>.current.delta.v1` instead.
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
- **The book is Gemini's, and it is thin.** The engine's book is a mirror of
  Gemini's public book for the contract, plus whatever pretend-money orders
  rest here. On the 5 and 15 minute contracts the spread is several cents
  wide, the top is often ten contracts, and one side is empty for part of
  many rounds. `<series>.current.price.v1` reports an empty bid as `0` and an
  empty ask as `1`, so a row that needs a real quote says so (`bid >= 0.03`,
  `ask <= 0.97`), and a document that means "the offers are gone" tests for
  exactly those values. A maker quotes inside the spread and never crosses
  it; a taker names the most it will pay. Because the best bid and ask
  include other pretend-money orders, two deployments of one maker can
  outbid each other a cent at a time; the room check (`ask - bid >= 0.03`)
  stops them two cents under the ask.

## Catalogue

| id | series | kind | reads |
|---|---|---|---|
| momentum-rider | both | taker | external micro_momentum, aggressor_flow |
| oracle-gap | both | taker | spot vs external spot_reference, strike |
| order-flow-lead | both | taker | external order_flow_imbalance, actionable_depth_imbalance |
| vwap-reversion | both | maker | external vwap_deviation, book |
| absorption-fade | btc-5m | taker | external absorption_state |
| inside-quote | both | maker, one cent inside the best on the side the strike favours (inverse quotes both sides when the strike says nothing) | book, strike distance, window clock |
| late-fade | both | taker in the last minute, small, against a side whose offers are gone (inverse joins it with a resting bid) | book, window clock |

`inside-quote` and `late-fade` read nothing but the exchange's own signals, so
they are ready as soon as a round is open, with every signal lane off. The rest
wait on the spot lane (Coinbase and Kraken).

How each one did against Gemini's live books is in the build notes
(`research/template-runs.md` beside the repository); the templates taken off
the public list as a result are named in the next section.

## Unlisted: compiled, not on the public list

These six templates carry `listed: false`: they still compile at boot and stay
reachable by id and deployable, but the list route leaves them out, because of
how each did against Gemini's live books. This is not the `parked/` folder
above, whose files are not embedded at all.

| template | run |
|---|---|
| `fair-value-discount` (Fair Price Finder) | Run sells inside the round and loses the spread every round; Inverse never sent an order. |
| `late-closer` (Buzzer Beater) | Run never sent an order; Inverse lost 27 rounds of 27 on the five-minute series and 8 of 8 on the fifteen. |
| `sweep-follower` (Whale Watch) | Both sides lost over 80 percent of the budget. |
| `polymarket-discount`, `polymarket-momentum` | Traded in 1 round of 11; the Polymarket signal is not bound to the round it matches. |
| `trend-rider` | Traded in 1 round of 11. It reads the spot momentum signals, not Polymarket: its trigger needs an 8 basis point move in a minute within the first five minutes of the round. |

The runs behind these verdicts are in `build-3/research/template-runs.md`
beside the repository.
