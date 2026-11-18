# Motion Markets — Wire Type Schemas

Single source of truth for every message that crosses a topology seam.
Modeled in **JSON Schema (draft 2020-12)**.

> **JSON Schema is the model, not the wire format.** Each link chooses its own
> codec — msgpack or protobuf-generated for control-plane messages over NATS, a
> dedicated binary codec for the `SignalUpdate` hot path — and validates against
> these schemas in tests. The model stays stable while the wire encoding is free
> to change per link.

## Conventions

- **`schema_version`** (integer, additive-only). Evolve a type by adding *optional*
  fields and bumping the version. Consumers must **tolerate unknown fields**.
- **Decimals as strings.** Prices/quantities/decimal signal values are strings to
  avoid float drift. Non-decimal signal values may be number/bool/string.
- **`additionalProperties: false`** for strictness; the additive mechanism above is
  how we extend without breaking readers.
- **`$comment`** marks provenance (which source artifact) and **reserved extension
  points** where a deferred decision will land.
- **Content-addressed `signal_id`.** For filtered (dynamic) signals, `signal_id`
  is derived — the hash of the canonicalized definition, not an assigned name. The
  exact procedure (algorithm, canonicalization, negative space) is normative in
  [`signal-id.md`](signal-id.md).

## Topology map

| Schema | Plane | Edge | MVP | Reserved for |
|---|---|---|---|---|
| `strategy-submission` | cloud | editor → pipeline | ✅ | — |
| `verification-result` | cloud | verify stage → editor | ✅ | — |
| `strategy-schema` | cloud | **compiler output — the contract** | ✅ single-stage | **Q2** `lifecycle_stages` |
| `strategy-artifact` | cloud | compile → store | ✅ | — |
| `deploy-message` | **cloud → datacenter (NATS)** | pipeline → runner pool | ✅ | **Q1** `deploy_epoch` |
| `signal-definition` | datacenter | signal registration | ✅ | histogram hints |
| `signal-update` | datacenter | source → engine *(hot path)* | ✅ | — |
| `strategy-registration` | datacenter | runner → engine | ✅ | **Q1** `epoch` |
| `activation` | datacenter | engine → runner | ✅ | — |
| `order-request` | datacenter | runner → execution | ✅ market | **Q1** `order_lifecycle_ref` |
| `execution-report` | datacenter | execution → runner | ✅ | — |
| `settlement-result` | datacenter | settler → runner | ✅ mock | — |

Shared `$defs` (Value, Comparator, Trigger, …) live in `common.schema.json`.

## Cloud → datacenter seam (NATS)

- `deploy-message` is published to a **JetStream** stream (`DEPLOY`, subject
  `deploy.strategy`) for durability.
- Runner instances subscribe as a **queue group** — JetStream delivers each deploy
  to exactly one runner, giving load balancing for free.
- A runner **acks after it has successfully registered** the strategy with its
  engine (at-least-once delivery; re-delivery on runner crash).
- Datacenter signals flow on core-NATS subjects `signal.<signal_id>` (ephemeral,
  fan-out) into the engine.

## Deferred decisions live *behind* these seams

- **Q1 — dynamic subscription.** `strategy-registration.epoch` +
  `deploy-message.deploy_epoch` make registration an idempotent, replaceable
  desired-state doc. MVP registers once; later, a higher epoch (or a future
  `subscription-update`) re-indexes the engine. The runner API and `activation`
  never change. The `execution-report` path back to the runner — wired in the MVP
  even though a static strategy ignores it — is what a lifecycle-aware strategy
  will consume to switch to cancel-conditions.
- **Q2 — lifecycle DSL.** `strategy-schema.lifecycle_stages` is reserved. When
  absent, `triggers` are the single active set (MVP). Later the compiler emits
  staged schemas and the engine grows a stage machine — registration and
  activation are untouched.
