// Kyros chain recorder.
//
// Reads pending records from the Kyros server over HTTP, writes each on chain to the
// StrategyRecord contract, waits one confirmation, and acknowledges it back to the server with the
// transaction hash. It is idempotent across restarts: before sending it reads `opened` / `isSealed`
// on chain and, if the record is already there (a crash after send but before ack), it finds the
// transaction hash from the event log and acks without sending again.
//
// Config comes from the environment only. The signing key (RECORDER_PRIVATE_KEY) is never logged,
// echoed, or written anywhere. Nothing here reads a keystore, a .env file, or ~/.config.
import { pathToFileURL } from "node:url";

import {
  BaseError,
  ContractFunctionRevertedError,
  createPublicClient,
  createWalletClient,
  defineChain,
  getAddress,
  http,
  isHex,
} from "viem";
import type { Address, Hex, PublicClient, WalletClient } from "viem";
import { privateKeyToAccount } from "viem/accounts";

// --- The only part of the contract ABI the recorder needs. ------------------------------------
export const STRATEGY_RECORD_ABI = [
  {
    type: "function",
    name: "open",
    stateMutability: "nonpayable",
    inputs: [
      { name: "deploymentId", type: "bytes32" },
      { name: "programHash", type: "bytes32" },
      { name: "budgetCents", type: "uint64" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "seal",
    stateMutability: "nonpayable",
    inputs: [
      { name: "deploymentId", type: "bytes32" },
      { name: "roundId", type: "uint64" },
      { name: "won", type: "bool" },
      { name: "pnlCents", type: "int64" },
      { name: "journalHash", type: "bytes32" },
    ],
    outputs: [],
  },
  {
    type: "function",
    name: "opened",
    stateMutability: "view",
    inputs: [{ name: "deploymentId", type: "bytes32" }],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    type: "function",
    name: "isSealed",
    stateMutability: "view",
    inputs: [
      { name: "deploymentId", type: "bytes32" },
      { name: "roundId", type: "uint64" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
  {
    type: "event",
    name: "Opened",
    inputs: [
      { name: "deploymentId", type: "bytes32", indexed: true },
      { name: "programHash", type: "bytes32", indexed: false },
      { name: "budgetCents", type: "uint64", indexed: false },
    ],
  },
  {
    type: "event",
    name: "Sealed",
    inputs: [
      { name: "deploymentId", type: "bytes32", indexed: true },
      { name: "roundId", type: "uint64", indexed: true },
      { name: "won", type: "bool", indexed: false },
      { name: "pnlCents", type: "int64", indexed: false },
      { name: "journalHash", type: "bytes32", indexed: false },
    ],
  },
] as const;

const ARC_CHAIN_ID = 5042002;
const LOCAL_CHAIN_ID = 31337;
const UINT64_MAX = (1n << 64n) - 1n;
const INT64_MIN = -(1n << 63n);
const INT64_MAX = (1n << 63n) - 1n;
// round_id is sent back to the server as Number(round_id) in the ack, so a value above
// Number.MAX_SAFE_INTEGER (2^53-1) would not round-trip: such a round is malformed, not merely large.
const MAX_SAFE_INT = BigInt(Number.MAX_SAFE_INTEGER);

// --- Records -----------------------------------------------------------------------------------
export type OpenRecord = {
  kind: "open";
  deploymentId: Hex;
  programHash: Hex;
  budgetCents: bigint;
};

export type SealRecord = {
  kind: "seal";
  deploymentId: Hex;
  roundId: bigint;
  won: boolean;
  pnlCents: bigint;
  journalHash: Hex;
};

export type StrategyRecordItem = OpenRecord | SealRecord;

export class ValidationError extends Error {}

/**
 * A record that cannot be written as-is and must NOT be retried: the contract rejected it (a revert
 * such as NotOpened / NotRecorder / AlreadySealed), its transaction was mined but reverted, or what
 * is already on chain differs from the pending item. The poller logs it, leaves it pending on the
 * server and moves on to the next item rather than blocking the queue behind it.
 */
export class UnsendableRecordError extends Error {}

const BYTES32 = /^0x[0-9a-fA-F]{64}$/;

function requireBytes32(value: unknown, field: string): Hex {
  if (typeof value !== "string" || !BYTES32.test(value)) {
    throw new ValidationError(`${field} must be a 0x-prefixed 32-byte hex string`);
  }
  return value as Hex;
}

// A JSON number is only safe to turn into a bigint when it is a *safe* integer: above 2^53 a double
// has already lost precision before we ever see it, so the value on the wire is not the value the
// server sent. Such a magnitude must arrive as a decimal string to round-trip exactly.
function toFieldBigInt(value: unknown, field: string): bigint {
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) {
      throw new ValidationError(`${field} must be a safe integer (use a string for values above 2^53-1)`);
    }
    return BigInt(value);
  }
  if (typeof value === "string" && /^-?\d+$/.test(value)) {
    return BigInt(value);
  }
  throw new ValidationError(`${field} must be an integer or an integer string`);
}

function requireUint(value: unknown, field: string, max: bigint): bigint {
  const v = toFieldBigInt(value, field);
  if (v < 0n || v > max) throw new ValidationError(`${field} out of range [0, ${max}]`);
  return v;
}

function requireInt(value: unknown, field: string, min: bigint, max: bigint): bigint {
  const v = toFieldBigInt(value, field);
  if (v < min || v > max) throw new ValidationError(`${field} out of range [${min}, ${max}]`);
  return v;
}

/** Validate and normalise one raw pending item. Throws ValidationError on anything malformed. */
export function validateRecord(raw: unknown): StrategyRecordItem {
  if (!raw || typeof raw !== "object") throw new ValidationError("record is not an object");
  const r = raw as Record<string, unknown>;
  const deploymentId = requireBytes32(r.deployment_id, "deployment_id");
  if (r.kind === "open") {
    return {
      kind: "open",
      deploymentId,
      programHash: requireBytes32(r.program_hash, "program_hash"),
      budgetCents: requireUint(r.budget_cents, "budget_cents", UINT64_MAX),
    };
  }
  if (r.kind === "seal") {
    if (typeof r.won !== "boolean") throw new ValidationError("won must be a boolean");
    return {
      kind: "seal",
      deploymentId,
      roundId: requireUint(r.round_id, "round_id", MAX_SAFE_INT),
      won: r.won,
      pnlCents: requireInt(r.pnl_cents, "pnl_cents", INT64_MIN, INT64_MAX),
      journalHash: requireBytes32(r.journal_hash, "journal_hash"),
    };
  }
  throw new ValidationError(`unknown kind ${JSON.stringify(r.kind)}`);
}

// --- Config ------------------------------------------------------------------------------------
export type Config = {
  apiUrl: string;
  rpcUrl: string;
  contractAddress: Address;
  privateKey: Hex;
  token: string;
  allowLocal: boolean;
  pollMs: number;
  backoffBaseMs: number;
  backoffMaxMs: number;
  maxAttempts: number;
  fromBlock: bigint;
  logChunk: bigint;
  // Upper bound on each HTTP request to the Kyros server (ms); an AbortSignal cancels a hung fetch.
  httpTimeoutMs: number;
  // Upper bound on waiting for one transaction receipt (ms).
  receiptTimeoutMs: number;
};

function requireEnv(env: NodeJS.ProcessEnv, name: string): string {
  const v = env[name];
  if (!v || v.trim() === "") throw new Error(`missing required env ${name}`);
  return v.trim();
}

function numEnv(env: NodeJS.ProcessEnv, name: string, fallback: number): number {
  const v = env[name];
  if (v === undefined || v.trim() === "") return fallback;
  const n = Number(v);
  if (!Number.isFinite(n) || n <= 0) throw new Error(`${name} must be a positive number`);
  return n;
}

function intEnv(env: NodeJS.ProcessEnv, name: string, fallback: number, min: number): number {
  const v = env[name];
  if (v === undefined || v.trim() === "") return fallback;
  const t = v.trim();
  if (!/^-?\d+$/.test(t)) throw new Error(`${name} must be an integer`);
  const n = Number(t);
  if (!Number.isSafeInteger(n) || n < min) throw new Error(`${name} must be an integer >= ${min}`);
  return n;
}

function bigIntEnv(env: NodeJS.ProcessEnv, name: string, fallback: bigint, min: bigint): bigint {
  const v = env[name];
  if (v === undefined || v.trim() === "") return fallback;
  const t = v.trim();
  if (!/^-?\d+$/.test(t)) throw new Error(`${name} must be an integer`);
  const n = BigInt(t);
  if (n < min) throw new Error(`${name} must be an integer >= ${min}`);
  return n;
}

/** Build config from the environment. Throws on anything missing or malformed. Never logs the key. */
export function loadConfig(env: NodeJS.ProcessEnv): Config {
  const privateKeyRaw = requireEnv(env, "RECORDER_PRIVATE_KEY");
  if (!isHex(privateKeyRaw) || privateKeyRaw.length !== 66) {
    throw new Error("RECORDER_PRIVATE_KEY must be a 0x-prefixed 32-byte hex key");
  }
  const contract = requireEnv(env, "STRATEGY_RECORD_ADDRESS");
  return {
    apiUrl: requireEnv(env, "KYROS_API_URL").replace(/\/+$/, ""),
    rpcUrl: requireEnv(env, "ARC_RPC_URL"),
    contractAddress: getAddress(contract),
    privateKey: privateKeyRaw as Hex,
    // The shared secret the server requires on every recorder request. Required; never logged.
    token: requireEnv(env, "RECORDER_TOKEN"),
    allowLocal: env.RECORDER_ALLOW_LOCAL === "1",
    pollMs: numEnv(env, "RECORDER_POLL_MS", 5000),
    backoffBaseMs: numEnv(env, "RECORDER_BACKOFF_BASE_MS", 2000),
    backoffMaxMs: numEnv(env, "RECORDER_BACKOFF_MAX_MS", 30000),
    maxAttempts: intEnv(env, "RECORDER_MAX_ATTEMPTS", 5, 1),
    fromBlock: bigIntEnv(env, "RECORDER_FROM_BLOCK", 0n, 0n),
    logChunk: bigIntEnv(env, "RECORDER_LOG_CHUNK", 9000n, 1n),
    httpTimeoutMs: numEnv(env, "RECORDER_HTTP_TIMEOUT_MS", 10000),
    receiptTimeoutMs: numEnv(env, "RECORDER_RECEIPT_TIMEOUT_MS", 60000),
  };
}

// --- Clients -----------------------------------------------------------------------------------
export type Clients = {
  publicClient: PublicClient;
  walletClient: WalletClient;
  account: ReturnType<typeof privateKeyToAccount>;
};

/** Create the viem clients. The account is derived from the key in memory; the key is never logged. */
export function createClients(config: Config): Clients {
  const account = privateKeyToAccount(config.privateKey);
  // retryCount 0: the recorder owns retry and backoff (see resolveTxHash); the transport must not
  // silently retry underneath it, or a failed send would never reach the backoff loop.
  const transport = http(config.rpcUrl, { retryCount: 0 });
  const publicClient = createPublicClient({ transport }) as PublicClient;
  const walletClient = createWalletClient({ account, transport });
  return { publicClient, walletClient, account };
}

/** Refuse to run against any chain but Arc Testnet (or the local chain when explicitly allowed). */
export async function verifyChainId(publicClient: PublicClient, config: Config): Promise<number> {
  const chainId = await publicClient.getChainId();
  const allowed = config.allowLocal ? [ARC_CHAIN_ID, LOCAL_CHAIN_ID] : [ARC_CHAIN_ID];
  if (!allowed.includes(chainId)) {
    throw new Error(
      `refusing to run: connected to chain ${chainId}, expected ${allowed.join(" or ")}` +
        (config.allowLocal ? "" : " (set RECORDER_ALLOW_LOCAL=1 to allow the local chain 31337)"),
    );
  }
  return chainId;
}

// --- Logging (never includes the key) ----------------------------------------------------------
export type Logger = {
  info: (msg: string) => void;
  warn: (msg: string) => void;
  error: (msg: string) => void;
};

export const consoleLogger: Logger = {
  info: (m) => console.log(m),
  warn: (m) => console.warn(m),
  error: (m) => console.error(m),
};

// --- Context -----------------------------------------------------------------------------------
export type RecorderContext = {
  config: Config;
  clients: Clients;
  chain: ReturnType<typeof defineChain>;
  log: Logger;
  // True once a shutdown (SIGINT/SIGTERM) has been requested. The retry loop and the per-item poll
  // loop both check it so a stop takes effect promptly instead of after the current backoff/queue.
  stopRequested: () => boolean;
};

export async function createContext(config: Config, log: Logger = consoleLogger): Promise<RecorderContext> {
  const clients = createClients(config);
  const chainId = await verifyChainId(clients.publicClient, config);
  const chain = defineChain({
    id: chainId,
    name: chainId === ARC_CHAIN_ID ? "arc-testnet" : `chain-${chainId}`,
    nativeCurrency: { name: "USDC", symbol: "USDC", decimals: 18 },
    rpcUrls: { default: { http: [config.rpcUrl] } },
  });
  return { config, clients, chain, log, stopRequested: () => false };
}

// --- HTTP to the Kyros server ------------------------------------------------------------------
export async function fetchPending(config: Config): Promise<unknown[]> {
  const res = await fetch(`${config.apiUrl}/api/records/pending`, {
    headers: { "x-recorder-token": config.token },
    signal: AbortSignal.timeout(config.httpTimeoutMs),
  });
  if (res.status === 401) throw new Error("the server refused the recorder's token on GET /api/records/pending");
  if (res.status === 503) throw new Error("the server has no recorder token configured (GET /api/records/pending → 503)");
  if (!res.ok) throw new Error(`GET /api/records/pending → ${res.status}`);
  const body = await res.json();
  if (!Array.isArray(body)) throw new Error("pending response is not an array");
  return body;
}

export async function ackRecord(config: Config, rec: StrategyRecordItem, txHash: Hex): Promise<void> {
  const body: Record<string, unknown> = { deployment_id: rec.deploymentId, tx_hash: txHash };
  if (rec.kind === "seal") body.round_id = Number(rec.roundId);
  const res = await fetch(`${config.apiUrl}/api/records/ack`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-recorder-token": config.token },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(config.httpTimeoutMs),
  });
  if (res.status === 401) throw new Error("the server refused the recorder's token on POST /api/records/ack");
  if (res.status < 200 || res.status >= 300) throw new Error(`POST /api/records/ack → ${res.status}`);
}

// --- On-chain reads / the event-log lookup -----------------------------------------------------
async function isPresent(ctx: RecorderContext, rec: StrategyRecordItem): Promise<boolean> {
  const { publicClient } = ctx.clients;
  const address = ctx.config.contractAddress;
  if (rec.kind === "open") {
    return publicClient.readContract({ address, abi: STRATEGY_RECORD_ABI, functionName: "opened", args: [rec.deploymentId] });
  }
  return publicClient.readContract({
    address,
    abi: STRATEGY_RECORD_ABI,
    functionName: "isSealed",
    args: [rec.deploymentId, rec.roundId],
  });
}

/**
 * The name of the first field in which a found event's arguments differ from the pending item, or
 * null if they match. Guards against acking a transaction that recorded different facts than the
 * server is asking us to record (hex compared case-insensitively; numbers as bigints).
 */
function differingField(rec: StrategyRecordItem, args: Record<string, unknown>): string | null {
  const sameHex = (a: unknown, b: Hex): boolean => typeof a === "string" && a.toLowerCase() === b.toLowerCase();
  if (rec.kind === "open") {
    if (!sameHex(args.programHash, rec.programHash)) return "programHash";
    if (args.budgetCents !== rec.budgetCents) return "budgetCents";
  } else {
    if (args.won !== rec.won) return "won";
    if (args.pnlCents !== rec.pnlCents) return "pnlCents";
    if (!sameHex(args.journalHash, rec.journalHash)) return "journalHash";
  }
  return null;
}

/**
 * Find the transaction hash that already recorded this item, from the contract's event log. Public
 * RPCs cap the block range of a single getLogs, so the scan of [fromBlock, latest] is split into
 * consecutive chunks of at most `logChunk` blocks, newest chunk first, stopping at the first hit.
 * The found event's arguments must match the pending item, or the item is unsendable (F5): acking a
 * transaction that recorded different facts would silently diverge the server from the chain.
 */
async function findTxHash(ctx: RecorderContext, rec: StrategyRecordItem): Promise<Hex> {
  const { publicClient } = ctx.clients;
  const address = ctx.config.contractAddress;
  const from = ctx.config.fromBlock;
  const chunk = ctx.config.logChunk;
  const latest = await publicClient.getBlockNumber();

  let hi = latest;
  while (hi >= from) {
    if (ctx.stopRequested()) throw new Error(`stop requested while looking for ${describe(rec)} on chain`);
    const lo = hi - chunk + 1n > from ? hi - chunk + 1n : from;
    const logs =
      rec.kind === "open"
        ? await publicClient.getContractEvents({
            address,
            abi: STRATEGY_RECORD_ABI,
            eventName: "Opened",
            args: { deploymentId: rec.deploymentId },
            fromBlock: lo,
            toBlock: hi,
          })
        : await publicClient.getContractEvents({
            address,
            abi: STRATEGY_RECORD_ABI,
            eventName: "Sealed",
            args: { deploymentId: rec.deploymentId, roundId: rec.roundId },
            fromBlock: lo,
            toBlock: hi,
          });
    const hit = logs.find((l) => l.transactionHash);
    if (hit?.transactionHash) {
      const field = differingField(rec, hit.args as Record<string, unknown>);
      if (field !== null) {
        throw new UnsendableRecordError(`on-chain record differs from the pending item: ${field} (${describe(rec)})`);
      }
      return hit.transactionHash;
    }
    if (lo === from) break;
    hi = lo - 1n;
  }
  throw new Error(`record is present on chain but no matching event was found for ${describe(rec)}`);
}

/**
 * Send the record on chain and wait for one confirmation. On a successful receipt returns our hash
 * (`sent: true`). If the receipt is not "success" the transaction was mined but reverted: if the
 * record is nonetheless present (a concurrent writer landed it) we return THAT transaction's hash
 * (`sent: false`, never our reverted one); otherwise the record is unsendable and we throw.
 */
async function sendRecord(ctx: RecorderContext, rec: StrategyRecordItem): Promise<{ txHash: Hex; sent: boolean }> {
  const { walletClient, publicClient, account } = ctx.clients;
  const base = { address: ctx.config.contractAddress, abi: STRATEGY_RECORD_ABI, account, chain: ctx.chain } as const;
  const hash =
    rec.kind === "open"
      ? await walletClient.writeContract({ ...base, functionName: "open", args: [rec.deploymentId, rec.programHash, rec.budgetCents] })
      : await walletClient.writeContract({
          ...base,
          functionName: "seal",
          args: [rec.deploymentId, rec.roundId, rec.won, rec.pnlCents, rec.journalHash],
        });
  const receipt = await publicClient.waitForTransactionReceipt({ hash, confirmations: 1, timeout: ctx.config.receiptTimeoutMs });
  if (receipt.status !== "success") {
    if (await isPresent(ctx, rec)) {
      const txHash = await findTxHash(ctx, rec);
      return { txHash, sent: false };
    }
    throw new UnsendableRecordError(`transaction ${hash} was mined but reverted for ${describe(rec)}`);
  }
  return { txHash: hash, sent: true };
}

function describe(rec: StrategyRecordItem): string {
  return rec.kind === "open"
    ? `open ${rec.deploymentId}`
    : `seal ${rec.deploymentId} round ${rec.roundId}`;
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// Sleep up to `ms`, in slices of at most 250 ms, returning early as soon as a stop is requested, so
// a shutdown is never held behind a long backoff or poll interval (G4).
async function interruptibleSleep(ms: number, stopRequested: () => boolean): Promise<void> {
  const SLICE_MS = 250;
  let remaining = ms;
  while (remaining > 0) {
    if (stopRequested()) return;
    const step = Math.min(SLICE_MS, remaining);
    await sleep(step);
    remaining -= step;
  }
}

/** The viem error name of a contract revert, if this error is one, else null (not a revert). */
function revertName(err: unknown): string | null {
  if (!(err instanceof BaseError)) return null;
  const revert = err.walk((e) => e instanceof ContractFunctionRevertedError);
  if (!(revert instanceof ContractFunctionRevertedError)) return null;
  return revert.data?.errorName ?? revert.reason ?? "revert";
}

/**
 * Resolve the transaction hash for a record, sending it if it is not already on chain. A failed
 * send is retried with exponential backoff (capped at backoffMaxMs) up to `maxAttempts` times per
 * item per poll, re-checking on-chain presence each attempt so a send that actually landed (or a
 * concurrent writer) is never repeated; after that it throws the last error. A contract *revert* is
 * never retried — it means the item cannot be written as-is — and surfaces as an
 * UnsendableRecordError. The loop also stops early when a shutdown has been requested.
 */
export async function resolveTxHash(ctx: RecorderContext, rec: StrategyRecordItem): Promise<{ txHash: Hex; sent: boolean }> {
  let attempt = 0;
  let lastErr: unknown;
  for (;;) {
    if (ctx.stopRequested()) {
      throw lastErr ?? new Error(`stop requested before recording ${describe(rec)}`);
    }
    try {
      if (await isPresent(ctx, rec)) {
        const txHash = await findTxHash(ctx, rec);
        if (attempt === 0) ctx.log.info(`already on chain, acking without sending: ${describe(rec)} (${txHash})`);
        return { txHash, sent: false };
      }
      return await sendRecord(ctx, rec);
    } catch (err) {
      if (err instanceof UnsendableRecordError) throw err;
      const name = revertName(err);
      if (name !== null) {
        throw new UnsendableRecordError(`refusing to retry ${describe(rec)}: contract reverted ${name}`);
      }
      lastErr = err;
      attempt += 1;
      if (attempt >= ctx.config.maxAttempts) {
        ctx.log.warn(`giving up on ${describe(rec)} after ${attempt} attempt(s): ${errMsg(err)}`);
        throw err;
      }
      const wait = Math.min(ctx.config.backoffBaseMs * 2 ** (attempt - 1), ctx.config.backoffMaxMs);
      ctx.log.warn(`send failed for ${describe(rec)} (attempt ${attempt}), retrying in ${wait}ms: ${errMsg(err)}`);
      await interruptibleSleep(wait, ctx.stopRequested);
    }
  }
}

/** Resolve (send or find) the transaction and ack it. An ack failure leaves it pending for next poll. */
export async function processRecord(ctx: RecorderContext, rec: StrategyRecordItem): Promise<void> {
  const { txHash, sent } = await resolveTxHash(ctx, rec);
  await ackRecord(ctx.config, rec, txHash);
  ctx.log.info(`${sent ? "recorded" : "re-acked"} ${describe(rec)} → ${txHash}`);
}

/** One poll cycle: fetch pending, validate each, process the valid ones in order. */
export async function pollOnce(ctx: RecorderContext): Promise<void> {
  let pending: unknown[];
  try {
    pending = await fetchPending(ctx.config);
  } catch (err) {
    ctx.log.warn(`failed to fetch pending: ${errMsg(err)}`);
    return;
  }
  // A deployment whose open is pending and was not recorded in this poll — it differs from the
  // record on chain, it could not be read, checked, sent or acked — has its seals held back for the
  // poll: not sent, not acked. Opens are tried first whatever order the server lists them in, and a
  // later poll in which the open goes through releases the seals.
  const openNotRecorded = new Set<string>();
  const records: StrategyRecordItem[] = [];
  for (const raw of pending) {
    try {
      records.push(validateRecord(raw));
    } catch (err) {
      ctx.log.warn(`skipping malformed record: ${errMsg(err)} — ${safeJson(raw)}`);
      const item = raw as { kind?: unknown; deployment_id?: unknown } | null;
      if (item && item.kind === "open" && typeof item.deployment_id === "string") {
        openNotRecorded.add(item.deployment_id.toLowerCase());
      }
    }
  }
  const ordered = [...records.filter((r) => r.kind === "open"), ...records.filter((r) => r.kind === "seal")];
  const warnedHeld = new Set<string>(); // one "held" line per poll per deployment
  for (const rec of ordered) {
    if (ctx.stopRequested()) break;
    const deployment = rec.deploymentId.toLowerCase();
    if (rec.kind === "seal" && openNotRecorded.has(deployment)) {
      if (!warnedHeld.has(deployment)) {
        warnedHeld.add(deployment);
        ctx.log.warn(`holding seals for ${rec.deploymentId}: its open was not recorded in this poll`);
      }
      continue;
    }
    try {
      await processRecord(ctx, rec);
    } catch (err) {
      if (rec.kind === "open") openNotRecorded.add(deployment);
      ctx.log.warn(`could not record ${describe(rec)}, leaving pending: ${errMsg(err)}`);
    }
  }
}

/**
 * Replace every http(s)/ws(s) URL in `text` with its origin only (scheme, host, port), dropping any
 * user-info, path, query and fragment. viem embeds the full RPC URL in its error messages
 * (`URL: http://host/v2/<key>`), so this keeps an API key or query secret out of every log line. A
 * URL that cannot be parsed is replaced with `<url>`.
 */
export function redactUrls(text: string): string {
  return text.replace(/(?:https?|wss?):\/\/[^\s]+/gi, (match) => {
    try {
      const u = new URL(match);
      return `${u.protocol}//${u.host}`;
    } catch {
      return "<url>";
    }
  });
}

function errMsg(err: unknown): string {
  return redactUrls(err instanceof Error ? err.message : String(err));
}

function safeJson(v: unknown): string {
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

// --- Entry point -------------------------------------------------------------------------------
export async function main(env: NodeJS.ProcessEnv = process.env, log: Logger = consoleLogger): Promise<void> {
  const config = loadConfig(env);
  const ctx = await createContext(config, log);
  log.info(`recorder up: signer ${ctx.clients.account.address}, contract ${config.contractAddress}, chain ${ctx.chain.id}`);

  let running = true;
  const stop = () => {
    running = false;
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
  ctx.stopRequested = () => !running;

  while (running) {
    await pollOnce(ctx);
    if (!running) break;
    await interruptibleSleep(config.pollMs, ctx.stopRequested);
  }
  log.info("recorder stopped");
}

const invokedDirectly = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedDirectly) {
  main().catch((err) => {
    console.error(`fatal: ${errMsg(err)}`);
    process.exit(1);
  });
}
