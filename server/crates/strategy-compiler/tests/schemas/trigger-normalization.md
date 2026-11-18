# Trigger Normalization — Tree → DNF

**Status:** normative.
**Applies to:** `common.schema.json` → `ConditionTree` / `Trigger.condition`.
**Authority:** the canonical implementation is the shared crate's normalization
pass; where prose and code disagree, **the code wins** — do not re-implement it.
Same rule, same reason as [`signal-id.md`](signal-id.md): a divergence here means
the same authored trigger registers differently on two systems, which corrupts
identity. (The crate doesn't exist yet; until it does, this doc is the spec of
record.)

## The split

The IR has two trigger formats, and the compiler is the bridge between them:

- **Authoring format — the tree.** `Trigger.condition` is a `ConditionTree`:
  arbitrary AND/OR/leaf nesting, exactly as the LLM emits it and the node graph
  renders it. English conditions arrive in arbitrary nesting; forcing the LLM to
  pre-distribute `(A OR B) AND (C OR D)` into groups is asking the least reliable
  component in the pipeline to do algebra.
- **Execution format — flat DNF groups.** The SME consumes an array of AND-groups
  (disjunctive normal form: OR of ANDs), each group a flat list of conditions. Its
  evaluation model is one bitmap per group, one bit per condition, "fire when any
  bitmap fills," with a condition shared across groups tracked once and fanned out.
  A tree-walker in the per-event hot path would buy nothing.

Normalization is the deterministic, semantics-neutral pass that turns the first
into the second. The compiler does this structural work **once at deploy time** —
the same principle as the harness consuming a pre-laid-out payload instead of
parsing one per activation.

## The pass

Input: one `ConditionTree`. Output: canonical DNF — a sorted list of sorted,
deduped, absorption-reduced AND-groups. Every step below applies to
**simultaneous** structure only; sequence atoms are exempt from all four (see
"Sequence atoms" below).

1. **Distribute** AND over OR until the formula is an OR of ANDs.
   `(A OR B) AND (C OR D)` → `{A,C}, {A,D}, {B,C}, {B,D}`.
2. **Dedupe within a group.** A condition repeated inside one AND-group is kept
   once — `{B, B, C}` → `{B, C}`.
3. **Absorption.** Drop any group that is a superset of another group: if `{B}`
   is a group, `{B, C}` is redundant because `B` alone already fires the trigger.
4. **Canonical sort.** Sort conditions within each group, then sort the groups,
   into one deterministic order (same ordering key as the hash canonicalization —
   `(signal_id/field, comparator, value)`).

There is no `not` node to handle: negation was pushed to the leaves by De Morgan
(comparator flip) before this pass ever runs — see the `ConditionTree` `$comment`.

## Sequence atoms — the one thing the pass must not touch

An `all`-node with `composite: "sequence"` is an **opaque atom** to this pass:

- **Never distributed into.** `(A seq B) AND (C OR D)` distributes *around* the
  atom — `{⟨A seq B⟩, C}, {⟨A seq B⟩, D}` — never through it.
- **Never sorted.** Sequence child order is semantic (become-true in array
  order); canonical sort applies to the *groups containing* the atom and to the
  atom's *position* in them, never to its children.
- **Never deduped or absorbed against its parts.** "B" does not absorb
  "A then B" — boolean absorption is truth logic; a sequence is a narrative. An
  atom participates in dedup/absorption only as a whole, compared by exact
  structural equality (same children, same order, same `within_ms`).

In the DNF output, a sequence atom occupies **one condition slot**: its
"sequence completed" boolean is a pseudo-condition in the group, backed by a
small SME automaton rather than a threshold entry. It counts as **one** toward
the per-group condition bound below, and its own child count is bounded by the
same ≤ 16 limit.

**Validator rule (v1):** a sequence node may not contain another sequence node,
at any depth. Simultaneous sub-trees inside a sequence step are fine (a step
latches when its sub-formula becomes true); nested narratives are CEP-grade
complexity nothing in the corpus needs.

## What each mode obligates the SME to do

Written down because each is easy to half-implement:

- **Simultaneous obligates negation tracking.** "All bits set right now" only
  means something if bits also *clear*. Registering `A >= 30` implicitly
  registers `A < 30` — every condition costs two threshold-ladder entries, one
  to set the bit, one to clear it. An implementation that only sets bits has
  silently built sticky semantics while claiming simultaneous ones, and the bug
  is invisible until a "both true now" trigger fires on facts an hour apart.
- **Sequence obligates a latch automaton with an explicit un-latch list.** Bits
  latch on becoming true, in order; a latched bit clears **only** on firing,
  re-arm, `within_ms` expiry, or leg exit — never on falsification. Latched
  bits are per-atom state: the cross-group "track a condition once, fan out to
  N bitmaps" dedup applies only to stateless snapshot conditions, never to a
  sequence atom's internals.
- **Expiry is not a firing.** When `within_ms` expires, all the atom's bits
  clear and the sequence may start over immediately — `rearm.cooldown_ms` does
  not apply to expiry. Decided here so it isn't a backtest discrepancy later.
- **Windows run on event time.** `within_ms` is measured between the first
  child's set-time and subsequent events' timestamps from the sequenced stream
  — never wall clock — so replay reproduces expiry exactly.

Disclosure: a sequence **without** `within_ms` can hold a partial latch
indefinitely ("whale trade, then a price move three hours later" fires). Legal,
but the decompilation must say so.

## The boundary: normalize syntax, not semantics

Distribution, dedup, absorption, and sorting are mechanical and safe. Anything
that recognizes semantic equivalence beyond absorption — e.g. proving
`B OR (A AND C)` equals some other formula by boolean algebra — is **out of
scope**. Dedup and compactness are optimizations; a subtle bug in anything
cleverer corrupts identity, the most load-bearing property in the system. Same
stopping rule as [`signal-id.md`](signal-id.md): boring syntactic normal form,
full stop. Do not "improve" it into a theorem prover.

Consequence, and it's fine: two triggers that are semantically equal but normalize
to different DNF (because absorption didn't reach them) get different canonical
forms and hash differently. That costs a duplicated evaluation, not a wrong
answer. Correctness never depends on catching those.

## Relationship to hashing

Canonical DNF **is** the hash input wherever trigger predicates get hashed.
Sorted, deduped, absorption-reduced DNF is far closer to a canonical form than an
arbitrary tree, where the same formula wears many syntactic outfits. The
normalization pass and the canonicalization-for-hashing step are the **same code
path** — this is the "one predicate grammar, one implementation" story stated
once and reused: the trigger layer and the filter/signal layer share it.

## Relationship to filters (the AND-group subset)

A filter (`signal-artifact.filter`) is a **single AND-group** — the stateless
leaf/AND subset of this grammar, never a tree. When a pushed-down predicate
contains an OR, normalization is what splits it: distribute to DNF, then each
group's stateless part becomes its own signal artifact. So an OR at the filter
layer is **two filters — two signals, two hashes** — which is strictly better for
dedup: another strategy sharing only one arm of the OR dedups against that arm
independently. Triggers are trees that normalize to DNF over such AND-groups;
filters are the individual groups. One pass serves both layers.

## The expansion bound (validator rule)

Distribution can blow up combinatorially: ANDing `n` OR-clauses of `k`
alternatives yields `k^n` groups. So the validator carries a checklist entry,
enforced **after normalization**:

- group count ≤ **32**
- per-group condition count ≤ **16** (generous)

Exceeding either is a collected, actionable draft-time error, not a silent
capacity failure:

> `this condition expands to 96 combinations — split into separate triggers or simplify.`

Real corpus triggers are 2–5 atoms and never approach the bound; it exists for the
pathological or LLM-hallucinated case, converting a runtime capacity problem into a
loud, fixable authoring-time one.

This is a **validator rule, not a schema constraint** — the `ConditionTree` schema
cannot see its own post-distribution expansion size; only the compiler can, after
normalization, which is exactly why the check lives there. The post-normalization
group count is also part of the trigger's true evaluation cost, so the **cost
stamp** reads it directly — the same number serves admission control for free.

