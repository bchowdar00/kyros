/**
 * What the writer must know to author a document that does what the user said. The
 * compiler proves a document is well formed; it cannot prove it means the sentence.
 * So this file teaches the machine that runs the document, in the order an activation
 * happens, and every fact carries the failure it prevents. The derivation procedure at
 * the end is what the writer runs on every brief.
 */
export const RULEBOOK = `# How a strategy runs

A strategy is a description that three parts of the exchange consult at different
moments. The Signals Matching Engine decides WHEN the strategy is allowed to think. The
interpreter decides WHAT it does with a wake, and decides everything before anything
touches the market. Order fates move the strategy between modes with no strategy code
running. Get the description slightly wrong and all three still cooperate perfectly, on
a different strategy than the user described.

The document is a small state machine. LEGS are the modes; the strategy is always in
exactly one, and starts in the first one listed. TRIGGERS are named facts about the
world with an edge policy. A leg's \`arm\` list is the set of triggers allowed to wake it.
BLOCKS are what a leg does on a wake, top to bottom. STATE slots are the only memory.
When the round settles, everything resets: the strategy returns to its first leg, every
state slot clears, every trigger's bookkeeping rewinds, and nothing is delivered until
the next round opens.

## Triggers are facts with an edge policy

A condition trigger is evaluated by the SME on every publish of a signal it reads and
produces a truth value. Per trigger, a tiny machine turns truth into fires:

  crossing  = truth and not previous_truth
  qualifies = fire == if_true ? truth : crossing
  if needs_false and not truth: needs_false = false
  fires     = qualifies and not spent and not needs_false
  on fire:  rearm never -> spent = true; fresh_crossing -> needs_false = true; immediate -> nothing
  previous_truth = truth
  a fire inside the cooldown is dropped, but the bookkeeping above already moved

Three rulings follow from this machine and they are the source of most wrong strategies:
- The bookkeeping advances whether or not the trigger is armed. A \`rearm: never\` trigger
  that goes true while the strategy is in another leg is spent for the round and will
  never fire.
- A crossing that happens while the trigger is not armed is lost, not queued. When the
  strategy later arms it, it waits for the NEXT crossing.
- Leg changes reset nothing. Only settlement rewinds the machine, and it sets
  previous_truth back to false, so a condition still true at the top of the next round
  counts as a fresh crossing on its first evaluation.

The phrasing of the user's sentence picks the policy:
- "when X" / "once X crosses": \`fire: on_crossing, rearm: fresh_crossing\`. Fires on the
  rising edge; needs a false in between to fire again.
- "whenever X" / "every time X": \`fire: if_true, rearm: immediate\`. This fires on EVERY
  evaluation while true, which is every book tick. It is a firehose and must be paced by
  a cooldown, a state gate the block flips, or a \`then\` out of the leg. Unpaced, it
  places an order per tick until the round budget refuses them.
- "once per round" / "at most once": \`rearm: never\`, remembering the first ruling.
- "every N seconds": an \`interval\` timer with \`fire: on_crossing, rearm: immediate\`.
- "N seconds before the close": a \`window_clock\` timer with \`fire: on_crossing,
  rearm: fresh_crossing\`.
- "N seconds after entering": an \`after\` timer with \`rearm: never\`.
- "on the way down but not on the way up": a SECOND trigger with the opposite comparator,
  armed only in the holding leg. Direction is a trigger, never a guard.
- A bare leaf \`{signal}\` is true on every publish; pair it with \`if_true, immediate\` to
  wake on every tick and put the real condition in the block guard when it reads state or
  the position.

Condition leaves: a bare leaf names a signal only; a field leaf compares one field to a
value; an expr leaf compares pure arithmetic over \`signal.<nick>.<field>\`, \`param.*\` and
\`envelope.*\` to a value. No comparisons, booleans, \`if\`, state, platform or event reads
inside an expr leaf: the SME evaluates it with zero strategy state. Group two or more
leaves with \`all\` (AND) or \`any\` (OR); never nest groups; there is no NOT, flip the
comparator. "Either side above 70 cents" is one \`any\` group with an up leaf and a down
leaf. Leaf values are text: a decimal ("0.6"), an enum variant ("oversold"), or with
value_kind \`param\` the name of a declared param. Time is always a timer, never a
comparison on the window signal's fields.

## Legs are masks, and timers restart on entry

\`arm\` is a delivery filter and nothing else. A fired trigger reaches the strategy only
if the current leg arms it. A trigger armed by no leg can never fire, and \`triggered(X)\`
in a leg that does not arm X is dead code. Every timer in the new leg's arm list is
re-armed the moment the leg is entered: \`after\` and \`interval\` measure from leg entry,
\`window_clock\` from the round's close. So an order timeout is an \`after\` timer armed in
the resting leg, and a stop-before-close timer must be armed in every active leg.

## Blocks run top to bottom, and then ends the walk

On a wake the interpreter walks the current leg's blocks in listed order. For each:
1. If any signal field the block reads has not published yet this round, the block is
   skipped silently.
2. The guard (\`guard\`) runs; absent means always. \`triggered(NAME)\` is true iff NAME is
   among what fired this wake and is legal only in a guard.
3. Statements write state immediately, so a later block in the same wake sees the write.
   Order intents place nothing yet: size and price are evaluated now, against the
   position and book as they were at the start of the wake.
4. \`next_leg\` (\`then\`) switches legs and STOPS the walk; blocks below it do not run this
   wake. The whole batch of orders then goes to the venue in order.

Consequences: a stop block above an entry block cannot both run on one wake; a counter
must be incremented in a block above the block that reads it; two orders in one block
are both sized before either is placed, so a flatten and a re-entry in the same block
both see the pre-flatten position, which is exactly what a flip needs.

Guards are boolean expressions over \`and\`, \`or\`, \`not\`, comparisons, arithmetic,
\`triggered(NAME)\`, the builtins abs, min, max, clamp, floor, ceil, round, and reducers
over windows (count, sum, max_of, min_of, oldest, newest). Platform reads:
platform.position.size (signed, up frame), platform.position.avg_cost,
platform.realized_pnl, platform.unrealized_pnl, platform.cash, platform.open_orders,
platform.window.remaining_ms, platform.last_fill.price. Fill reads:
\`legs.<leg>.filled_qty\`, \`legs.<leg>.fill_price\`, \`legs.<leg>.fills\`, and per tag
\`legs.<leg>.tag.<tag>.fill_price\`. \`event.<field>\` reads the publishing signal's payload
and is legal only when the guard contains exactly one \`triggered(NAME)\` and NAME is a
condition on one signal; otherwise read \`signal.<nick>.<field>\`. Durations in
expressions are written like \`15s\`.

Statements: \`set state.<slot> = <expr>\` and \`push state.<window> = <expr>\`. Declare slots
in \`state\`; scalars default to zero, false or the empty string.

## Two kinds of transition

A \`next_leg\` is YOUR decision, made at placement time, before anyone knows whether the
order filled. An on-route (\`on_fill\`, \`on_partial\`, \`on_reject\`, \`on_cancel_ack\`,
\`on_expiry\`) is the MARKET's decision, delivered later and applied mechanically; it
belongs to the order and still applies after you have moved legs. Leave a fate null to
stay. The fate alphabet is exact:
- fill: the whole order filled. partial: some filled, the rest still live or dead.
- reject: the venue or the risk gate refused it; nothing happened.
- cancel_ack: a cancel you sent (or the settlement) removed it.
- expiry: an immediate order that got nothing, or a resting order past expire_after.
An unfilled IOC is an EXPIRY, never a reject. So an entry that moves with \`next_leg\`
must also route \`on_expiry\` back to the hunting leg, or a no-fill strands the round in
holding with no position. A resting order routes \`on_fill\` (and usually \`on_partial\`)
to the holding leg and is pulled by a timer.

## Sides: up and down are the quoted side and its complement

Every market has two sides and the book quotes the first. This rulebook calls them up and
down because the BTC series do; an NBA series calls them home and away, and its book's
fields are \`home\`, \`away\`, \`bid\`, \`ask\` and \`last\` in the home frame. The prompt tells
you the labels of the market you are writing for. Read every up in these rules as the
quoted side and every down as its complement, and write the market's own labels in
\`outcome\`: the compiler checks them against the registry verbatim and refuses \`up\` on a
market whose sides are home and away.

## Positions are signed in the up frame

The book is one canonical up book. Buying \`down\` is selling \`up\` at the complement, so
"holding down" is a NEGATIVE platform.position.size and its average cost is an up price.
\`reduce_only\` clamps the order to what it closes on its side and REJECTS when there is
nothing to close on that side. Therefore:
- Flatten a long (position > 0): sell up, market, ioc, reduce_only.
- Flatten a short (position < 0): buy up, market, ioc, reduce_only.
- Do not flatten a down position with sell up: reduce_only rejects it.
- Flatten size is the whole position: \`clamp(abs(platform.position.size), 0,
  envelope.max_position_per_market)\` with \`max_size\` \`envelope.max_position_per_market\`.
- A flip is two orders in one block: the flatten of the current side, then a take of the
  other side sized in the dollars just released:
  \`clamp(floor(abs(platform.position.size) * platform.position.avg_cost / (1 - signal.book.bid + 0.01)), 0, envelope.max_order_size)\`
  for flipping from up to down (divide by \`signal.book.ask + 0.01\` when flipping to up).
- \`target_position\` with size 0 and reduce_only reaches flat with one market order
  whichever side is held; use it when the side is not known at authoring time.

## Orders

- A market take is an immediate limit a cent through the book, never a bare market
  order: buying up is \`order_type: limit, tif: ioc\` at \`clamp(signal.book.ask + 0.01,
  0.01, 0.99)\`; buying down is the same at \`clamp(1 - signal.book.bid + 0.01, 0.01,
  0.99)\`. The risk gate costs a limit at its price and a bare market order at one dollar
  a contract, which would eat the round budget. Only a flatten is a bare market order.
- A limit order carries a clamp-wrapped \`price\` and a \`price_band\` whose bounds equal
  the clamp's literal bounds. \`tif: gtc\` rests; \`tif: ioc\` fills what it can and dies.
- Never set post_only or expire_after; use an \`after\` timer with a cancel instead.
- Every order carries a \`tag\`; \`cancel\` and \`cancel_replace\` act on a tag. A
  cancel-replace is the maker's requote and is self-limiting; it needs no pacing.
- \`instrument\` is always \`market\`; \`outcome\` is the quoted side or its complement in the
  market's own labels (\`up\` or \`down\` on BTC, \`home\` or \`away\` on NBA).

## Money

The strategy never sees the allocation. Each round it is handed \`envelope.round_budget\`
in dollars and the risk gate refuses, never shrinks, any order that would commit more in
a round: this round's realized loss + the open position's cost + resting opening
notional + this order's opening cost may not exceed it. Closing orders are free. The
budget re-arms at settlement.
- A \`size\` is clamp-wrapped and its value (the clamp's first argument) must scale with
  \`envelope.*\` or with the position; a literal contract count is refused.
- Size an entry in dollars over the price you will pay, with the brief's trade share as
  the literal (here 0.25): buying up at ask + 0.01:
  \`clamp(floor(envelope.round_budget * 0.25 / (signal.book.ask + 0.01)), 0, envelope.max_order_size)\`;
  buying down: divide by \`(1 - signal.book.bid + 0.01)\`; a resting bid at price P
  divides by P. \`max_size\` on an entry is \`envelope.max_order_size\`. A size that floors
  to zero is a local reject and follows on_reject.
- The trade share must equal the brief's trade share on every entry; it also sets the
  envelope's per-order cap. A strategy that plans N entries per round uses about 1/N
  each; a single shot may use the whole round budget (1). A strategy that fires on every
  publish or on an interval must not outrun the budget: pace the trigger or size each
  fire so the planned count fits, because a refused order is a wasted wake.

## Signals and market facts

- Declare every signal you read under a nickname and refer to it as
  \`signal.<nick>.<field>\`. Only listed fields exist. Use the ids exactly as given.
- \`<series>.current.price.v1\` is the book: the quoted side and its complement under
  their own names (up, down on BTC; home, away on NBA), then bid, ask, last (quoted side).
- \`<series>.current.window.v1\` is the round clock; wake on it only as a bare leaf.
- One instrument, nicknamed \`market\`, id \`<series>.current\`. Prices are probabilities
  from 0.01 to 0.99; a contract pays 1 on the winning side.

## Identifiers and numbers

- Names match [a-zA-Z][a-zA-Z0-9_]* and must not be: and, or, not, if, then, else, true,
  false, set, push, event, signal, state, platform, legs, expr, table, entry, envelope,
  param, stay, or a builtin name.
- Decimals are canonical strings: "0.6" not "0.60" or ".6". Durations are "10s", "500ms",
  "2m". Expressions are single-line ASCII with no double quotes.
- Group two or more leaves; a single leaf uses group \`single\`. Interval timers are at
  least 250ms and never \`rearm: never\`.

## The derivation procedure

Run these steps on every brief, in this order, before writing any JSON. The plan you
were given (when one is given) is the output of steps 1 to 5; follow it exactly.
1. Situations become legs. List every situation the strategy can be in: hunting,
   holding up, holding down, resting, done. One leg each. "Flip" means two holding legs.
   "Add on the way down" means one holding leg with a counter. "Quote both sides" means
   one quoting leg with two tags.
2. Per leg, what may wake it. Each is a trigger armed in that leg only. The stop timer
   is armed in every leg that can still place.
3. Per trigger, the edge policy from the phrasing table above.
4. Per wake, the action and where it leads. A take moves with \`next_leg\` and routes
   \`on_expiry\` and \`on_reject\` back. A resting order stays and routes \`on_fill\` forward.
   A flatten moves to done or back to hunting for a re-entry. A flip moves to the other
   holding leg.
5. What must be remembered across wakes: entry price, count of adds, whether a side was
   already taken. Each is a state slot or a \`legs.*\` read.
6. Sizes: dollars over the price paid with the brief's trade share; flattens sized off the
   position; a flip's second order sized off the dollars released.
7. Fates: every take covers expiry and reject; every resting order covers fill and is
   pulled by a timer; no on-route names a leg that does not exist.

## Shape

- Every leg that is not \`done\` is \`resident: true\`. \`done\` arms nothing.
- Stop trading before the close with a window_clock timer (usually 10s) armed in every
  active leg, moving to \`done\` after cancelling any resting tag.
- \`description\` restates the whole document in plain trading language: the situations,
  what wakes each, what it does, how it exits, how often it repeats.`
