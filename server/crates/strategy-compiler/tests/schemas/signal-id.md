# `signal_id` — Content-Addressing Specification

**Status:** normative.
**Applies to:** `signal-artifact.schema.json` → `signal_id`.
**Authority:** this document describes the procedure; the canonical implementation
is the shared crate's `signal_id()`. Where prose and code disagree, **the code
wins** — do not re-implement this procedure in a second place, call the crate.
(The crate doesn't exist yet; until it does, this doc is the spec of record. The
number rules and sort below are the drift-prone parts — ship the crate early.)

## What this is

A convention, not a schema. `signal-artifact` *declares* that `signal_id` is a
content hash; this spec defines *exactly how* it's computed, so that the compiler,
the SME (validating on receipt), and any backtest service derive **byte-identical**
IDs from the same definition. The ID is derived, never assigned.

## Why content-addressing (and why assigned IDs fail)

Two properties everything downstream leans on:

- **Dedup.** Two users writing the same filter ("LeBron missed FTs") must produce
  the *same* ID, so the SME evaluates it once and fans out to both. Assigned IDs
  (user-picked names, sequential handouts) give the same filter two IDs → dedup
  needs a registry comparing definitions, with all the drift and availability
  problems registries bring.
- **Immutable meaning.** An ID must refer to exactly one computation, forever.
  With assigned IDs, nothing stops someone "fixing" a filter under a stable ID —
  silently changing the meaning of every frozen strategy and historical backtest
  that references it. That is precisely the backtest-divergence failure the whole
  versioning discipline exists to kill.

Content addressing collapses both to one rule: **the ID is the definition.**
Changing the definition changes the ID; drift isn't forbidden, it's
unrepresentable. This only holds if everyone hashes *identically* — hence
canonicalization pinned to the byte.

## The algorithm

```
signal_id = lowercase_hex( SHA-256( canonical_bytes ) )
```

`canonical_bytes` = the UTF-8 serialization of:

```json
{
  "feed": "<string>",
  "feed_version": "<string>",
  "filter": "<canonicalized filter>",
  "normalization_version": "<string>"
}
```

serialized as JSON with:

- object keys sorted lexicographically, **all levels**
- no insignificant whitespace
- numbers in canonical form (see below)

## Filter canonicalization

Applied to the `filter` array *before* serialization:

1. **Sort** the AND-conditions by `(field, comparator, value)` — so `[A, B]` and
   `[B, A]` hash identically. AND is commutative; clause order is not identity.
2. **Comparators — one spelling per comparison.** The current enum
   (`>= <= > < == !=`) is already collision-free. The rule bites when ranges /
   expressions arrive: never allow two encodings of the same test.
3. **Canonical numbers** — the rule most likely to be implemented inconsistently;
   pin it hardest. Decimal values are strings (per `common`'s `SignalValue`
   guidance), in one fixed format:
   - no leading `+`
   - leading zero for magnitudes < 1 — `0.5`, never `.5`
   - no leading zeros on the integer part — `5`, never `05`
   - no trailing fractional zeros — `0.5`, never `0.50`
   - integers carry no decimal point — `5`, never `5.0`
   - no exponent notation — `500000`, never `5e5`
   - operate on the decimal string **lexically — never parse to float**. Float
     parsing reintroduces the exact drift that decimals-as-strings exist to avoid.
4. **Fold constants** — reserved. N/A until expressions can appear in filters
   (Tier 3); today's filter values are literals only. Reserve the step; don't
   implement it.

## The boundary: canonicalize syntax, never semantics

Normalize *how a filter is written*, never *what it means*.

- **Same hash (syntax):** `notional >= 500000 AND result == "missed"` vs. the same
  two clauses reordered.
- **Different hash — and that's correct (semantics):** `NOT (a OR b)` vs.
  `(NOT a) AND (NOT b)`; `x > 5 AND x > 3` vs. `x > 5`.

Dedup is an optimization, not a correctness property. Two semantically-equal but
syntactically-different filters costing two evaluations wastes nanoseconds; a
semantic-equivalence engine is a rabbit hole of genuinely subtle bugs — and a
subtle bug in canonicalization corrupts *identity*, the most load-bearing property
in the system. Boring syntactic normal form, full stop. **Do not "improve" this
into semantic normalization.**

## Negative space — what is excluded from the hash

Excluding wrongly makes distinct things collide; including wrongly makes identical
things diverge (imagine hashing a `created_at` — every upsert becomes a "new"
signal and dedup dies). The hash covers **exactly** the four fields above.
Explicitly excluded:

- `schema_version` — transport concern, not identity
- any future display-name / description field
- refcounts
- timestamps (`created_at`, deploy time, anything wall-clock)

**Default rule:** any field added to `signal-artifact` later is excluded from the
hash unless a reviewer deliberately decides identity depends on it.

## Room for improvement

Ordered roughly by how much they'd hurt if ignored.

- **Grammar / evaluation-semantics version isn't in the hash — sharpest gap.**
  The hash covers feed parsing (`normalization_version`) but not the
  *interpretation* of the filter grammar itself. If `common`'s comparator or value
  semantics ever change, identical filter bytes could mean different things across
  grammar versions yet collide on one ID — silent divergence, the failure mode
  content-addressing exists to prevent. Fix: either fold a `grammar_version` into
  the hashed object, or widen `normalization_version`'s definition to explicitly
  cover evaluation semantics, not just parsing. Decide before the grammar changes
  under live signals.
- **String normalization (Unicode) is unspecified.** Categorical string values
  (`"missed"`, player names, team names) are hashed as raw UTF-8 bytes. Two
  upstream encodings that render identically but differ in Unicode normalization
  (NFC vs NFD) fork the hash. Pin a normalization form — recommend NFC — before
  non-ASCII values flow through.
- **Opaque string fields are hashed verbatim.** `feed`, `feed_version`,
  `normalization_version` get no normalization — a stray space or casing
  difference mints a distinct signal. Validate/normalize them at their source of
  truth (a feed registry) so typos can't create phantom signals.
- **Hash length / truncation undecided.** SHA-256 → 64 lowercase hex chars. Fine
  as-is; if IDs land in hot paths or logs often, decide a truncation length *once*
  and document the collision domain, rather than letting each caller truncate
  differently.
- **This wants to be code, not prose.** Steps 1 and 3 are exactly where two
  independent implementations drift. Treat this doc as the spec of record and the
  shared crate as the authority (see header); ship the crate before a third
  service needs to hash.
