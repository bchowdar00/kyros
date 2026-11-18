# Kyros chain: StrategyRecord + recorder

A small on-chain record of Kyros activity on **Arc Testnet (chain id 5042002)**, plus a recorder
service that mirrors the Kyros server's pending records onto it.

## What it records — and what it does not

`StrategyRecord.sol` is an append-only, on-chain ledger of two facts:

- **Each strategy deployment** (`open`): its `deploymentId`, the hash of its compiled program, and
  the budget it commits, in cents.
- **Each settled round** (`seal`): per `(deploymentId, roundId)`, whether it won, its realised PnL
  in cents (signed), and the hash of the journal that attests the outcome.

It deliberately stores **no market price of any kind** — not the strike, not the spot, not a fill
price, nowhere, in no field. It records only what a deployment commits to and what its own journal
attests. There is no owner, no upgradeability, no payable path, and no external call. The single
writer is the `recorder` address fixed in the constructor (immutable); anyone can read a record back.

## Contract interface

```solidity
function open(bytes32 deploymentId, bytes32 programHash, uint64 budgetCents) external;      // onlyRecorder, once per id
function seal(bytes32 deploymentId, uint64 roundId, bool won, int64 pnlCents, bytes32 journalHash) external; // onlyRecorder, once per (id, round)

function opened(bytes32 deploymentId) external view returns (bool);
function isSealed(bytes32 deploymentId, uint64 roundId) external view returns (bool);

event Opened(bytes32 indexed deploymentId, bytes32 programHash, uint64 budgetCents);
event Sealed(bytes32 indexed deploymentId, uint64 indexed roundId, bool won, int64 pnlCents, bytes32 journalHash);

error NotRecorder();   // caller is not the recorder
error AlreadyOpened(); // this deployment was already opened
error NotOpened();     // seal before open
error AlreadySealed(); // this (deployment, round) was already sealed
```

> **Note on `isSealed`.** The task specified the reader as `sealed(bytes32,uint64)`, but `sealed`
> is a reserved keyword in Solidity and will not compile as a function name. The reader is therefore
> named **`isSealed`**. `opened` is not reserved, so it keeps its specified name.

`seal` reverts `NotOpened` unless the deployment was opened first; a repeat `open` or a repeat
`seal` for the same round reverts. Negative PnL is a plain `int64`, so losses round-trip.

## Toolchain

Pinned versions:

- Solidity **0.8.38**, optimizer on (200 runs), EVM `prague`.
- Hardhat **3.22.0** with `@nomicfoundation/hardhat-toolbox-viem` **5.0.7**, viem **2.57.19**,
  TypeScript **6.0.3**, `@types/node` **24.19.2**.

## Running the tests

```bash
cd chain
npm install
npm test          # builds, runs the contract tests, then the recorder tests
# or individually:
npm run build          # hardhat build
npm run test:contract  # hardhat test nodejs (in-memory chain)
npm run test:recorder  # node --test against a spawned `hardhat node` + a fake HTTP server
```

The contract tests run on Hardhat's in-memory chain. The recorder tests spawn a local
`hardhat node` (chain 31337) and a fake HTTP server implementing the Kyros records interface; they
sign with Hardhat's built-in account 0 (a public, deterministic test key — never a real one).

## The recorder

`recorder/src/index.ts` polls the Kyros server for pending records every 5 s, writes each on chain,
waits one confirmation, and acks it back with the transaction hash. It is idempotent across
restarts: before sending it reads `opened` / `isSealed` and, if the record is already there (a crash
after send but before ack), it finds the transaction hash from the event log — scanning
`[RECORDER_FROM_BLOCK, latest]` in chunks of at most `RECORDER_LOG_CHUNK` blocks, newest first — and,
after confirming the on-chain record matches the pending item, acks without sending again. A
transient send failure is retried with exponential backoff (with the default 5 attempts the waits
are 2, 4, 8 and 16 s; the 30 s cap applies only when the attempt count is raised) up to
`RECORDER_MAX_ATTEMPTS` times per item per poll, then given up (left pending for the next poll). A
contract revert (e.g. the signer is not the recorder, or a seal arrives before its open) is never
retried: it is logged, left pending, and the queue moves on to the next item. A record already on
chain with different facts is not a revert — it is detected by comparing the event already on chain
with the pending item; the item is left pending. Opens are tried before seals whatever order the
server lists them in. While a deployment's `open` is in the pending list and was not recorded in the
current poll (it is malformed, it differs from the record on chain, or it could not be checked, sent
or acked), that deployment's seals are held back for the poll: not sent and not acked. A later poll
in which the open goes through releases them. A transaction that is mined but reverts is never acked. An error while waiting
for a receipt can lead to a second transaction for the same record, which then reverts; the first
transaction's hash is the one acked, and the cost is the wasted gas. Malformed pending items are
skipped and logged; the rest proceed. It refuses to start unless the RPC reports chain id 5042002
(or 31337 when `RECORDER_ALLOW_LOCAL=1`, for tests). On SIGINT/SIGTERM it finishes the attempt in
hand for the current item (at most one send, one receipt wait and one ack, each bounded by the
timeouts below; RPC reads are bounded by viem's ten-second default) and starts no new item and no
new retry. The search for a record already on chain stops between chunks, and backoff and poll
sleeps end within a quarter second of the stop.

### Server HTTP interface it consumes

- `GET {KYROS_API_URL}/api/records/pending` → an array of records
  `{ kind: "open"|"seal", deployment_id, program_hash?, budget_cents?, round_id?, won?, pnl_cents?, journal_hash? }`
- `POST {KYROS_API_URL}/api/records/ack` with `{ deployment_id, round_id?, tx_hash }` → any 2xx
  (an `open` is acked without `round_id`).

Both routes are closed unless the request carries the header `x-recorder-token` equal to the
server's `RECORDER_TOKEN`: a missing or wrong token gets `401`, and a server with no token
configured gets `503` (both with a JSON `{ "error": ... }` body). The recorder sends the header on
every request; a `401`/`503` is an ordinary poll error that it logs and retries on its normal
schedule.

### Running the recorder

There is no npm start script; run the entry point directly (Node runs the `.ts` by type-stripping),
with its configuration in the environment:

```bash
cd chain
KYROS_API_URL=https://kyros.example \
ARC_RPC_URL=https://rpc.testnet.arc.io \
STRATEGY_RECORD_ADDRESS=0x... \
RECORDER_PRIVATE_KEY=0x... \
node recorder/src/index.ts
```

`RECORDER_PRIVATE_KEY` and `RECORDER_TOKEN` are secrets: export them into the host's environment
beforehand rather than writing them on the command line as the other values are shown above.

**Run exactly one recorder at a time**: two instances share the one signing key and would race for
the same nonce, so one of each pair of transactions would be dropped or replaced.

### Recorder environment values

Required (the process exits if any is missing or malformed):

| Name | Meaning |
|---|---|
| `KYROS_API_URL` | base URL of the Kyros server |
| `ARC_RPC_URL` | Arc Testnet JSON-RPC URL (`https://rpc.testnet.arc.io`) |
| `STRATEGY_RECORD_ADDRESS` | deployed `StrategyRecord` address |
| `RECORDER_PRIVATE_KEY` | the recorder's signing key (never logged or echoed) |
| `RECORDER_TOKEN` | shared secret sent as `x-recorder-token` on every recorder request; must equal the server's `RECORDER_TOKEN`. A secret — set it in the host's environment, never in a file or a command argument |

Optional (with defaults):

| Name | Meaning | Default |
|---|---|---|
| `RECORDER_ALLOW_LOCAL` | `1` to permit the local chain 31337 (tests only) | unset (Arc only) |
| `RECORDER_POLL_MS` | poll interval, ms (> 0) | `5000` |
| `RECORDER_BACKOFF_BASE_MS` | first retry backoff, ms (> 0) | `2000` |
| `RECORDER_BACKOFF_MAX_MS` | backoff cap, ms (> 0) | `30000` |
| `RECORDER_MAX_ATTEMPTS` | send attempts per item per poll before giving up (integer ≥ 1) | `5` |
| `RECORDER_FROM_BLOCK` | first block of the event-log scan — the contract's deploy block (integer ≥ 0) | `0` |
| `RECORDER_LOG_CHUNK` | max blocks per `getLogs` request when scanning for an event (integer ≥ 1) | `9000` |
| `RECORDER_HTTP_TIMEOUT_MS` | per-request timeout for each HTTP call to the Kyros server, ms (> 0) | `10000` |
| `RECORDER_RECEIPT_TIMEOUT_MS` | timeout for waiting on one transaction receipt, ms (> 0) | `60000` |

After a deploy, set `RECORDER_FROM_BLOCK` to the block of the deploy transaction, so the idempotency
log scan starts at the contract's first block instead of genesis (public RPCs reject or truncate a
scan that reaches back further than needed).

The key is supplied through the environment only. Nothing here reads a keystore, a `.env` file, or
`~/.config`, and the key never appears in any log line.

## Deploying (owner steps — run against Arc Testnet by the owner, not in CI)

Arc Testnet uses **test USDC as its gas token** (there is no separate native coin).

1. Put the deployer key in Hardhat's encrypted keystore (it prompts; the key never goes in a file
   or on a command line):
   ```bash
   cd chain
   npx hardhat keystore set ARC_DEPLOYER_KEY
   ```
2. Fund the deployer address with Arc Testnet test USDC from the faucet at **https://faucet.circle.com**
   (choose Arc Testnet). This USDC pays for the deployment gas.
3. Deploy, with the recorder's address (an address, not a key) in the environment:
   ```bash
   RECORDER_ADDRESS=0x... npx hardhat run scripts/deploy.ts --network arcTestnet
   ```
   The script refuses any chain but 5042002, prints the deployer and the recorder address it will
   set before sending, and prints the deployed address after.
4. Verify on the explorer: **https://explorer.testnet.arc.io**

RPC `https://rpc.testnet.arc.io`; explorer `https://explorer.testnet.arc.io`.

## Deployed address

Deployed address: (not yet deployed)
