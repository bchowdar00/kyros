// Recorder integration tests. They run against a freshly-spawned `hardhat node` (chain 31337) and
// a fake HTTP server that implements exactly the Kyros records interface. No real key is used: the
// recorder signs with Hardhat's built-in account 0, derived from the well-known test mnemonic
// ("test test test ... junk") — a public, deterministic test key, never a real one.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import http from "node:http";
import net from "node:net";
import { readFileSync } from "node:fs";
import { after, before, describe, it } from "node:test";

import {
  createPublicClient,
  createWalletClient,
  defineChain,
  http as viemHttp,
  bytesToHex,
  keccak256,
  toHex,
} from "viem";
import type { Address, Hex, PublicClient } from "viem";
import { mnemonicToAccount } from "viem/accounts";

import {
  STRATEGY_RECORD_ABI,
  ackRecord,
  createContext,
  fetchPending,
  loadConfig,
  pollOnce,
  redactUrls,
  resolveTxHash,
  validateRecord,
  ValidationError,
  verifyChainId,
  createClients,
} from "../src/index.ts";
import type { Config, Logger, RecorderContext, StrategyRecordItem } from "../src/index.ts";

// --- Fixed test material (all invented) --------------------------------------------------------
const MNEMONIC = "test test test test test test test test test test test junk";
const hd0 = mnemonicToAccount(MNEMONIC, { addressIndex: 0 });
const RECORDER_KEY = bytesToHex(hd0.getHdKey().privateKey!) as Hex;

const chainDir = new URL("../../", import.meta.url);
const recorderEntry = new URL("../src/index.ts", import.meta.url);
const artifact = JSON.parse(
  readFileSync(new URL("../../artifacts/contracts/StrategyRecord.sol/StrategyRecord.json", import.meta.url), "utf8"),
) as { abi: unknown[]; bytecode: Hex };

const rpcUrl = (port: number) => `http://127.0.0.1:${port}`;
const delay = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function localChain(port: number) {
  return defineChain({
    id: 31337,
    name: "hardhat-local",
    nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [rpcUrl(port)] } },
  });
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const port = (s.address() as net.AddressInfo).port;
      s.close(() => resolve(port));
    });
  });
}

async function rpcChainId(port: number): Promise<number> {
  const res = await fetch(rpcUrl(port), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "eth_chainId", params: [] }),
  });
  const j = (await res.json()) as { result: string };
  return parseInt(j.result, 16);
}

async function waitUntil(pred: () => Promise<boolean> | boolean, timeoutMs: number, stepMs = 50): Promise<void> {
  const start = Date.now();
  for (;;) {
    if (await pred()) return;
    if (Date.now() - start > timeoutMs) throw new Error("waitUntil timed out");
    await delay(stepMs);
  }
}

async function startNode(port: number): Promise<ChildProcess> {
  // `npx hardhat node` spawns grandchildren and its stdio pipes keep the test's event loop alive,
  // so: ignore stdio (readiness is polled over RPC) and start a new process group we can signal
  // whole. Readiness is confirmed by the RPC answering with chain 31337.
  const child = spawn("npx", ["hardhat", "node", "--port", String(port)], {
    cwd: chainDir,
    stdio: "ignore",
    detached: true,
  });
  await waitUntil(async () => {
    try {
      return (await rpcChainId(port)) === 31337;
    } catch {
      return false;
    }
  }, 30000);
  return child;
}

/** Kill a child (and its whole process group, when detached) and wait for it to exit. */
async function stopChild(child: ChildProcess | undefined, signal: NodeJS.Signals = "SIGTERM"): Promise<void> {
  if (!child || child.exitCode !== null) return;
  const killAll = (sig: NodeJS.Signals) => {
    try {
      if (child.pid) process.kill(-child.pid, sig); // the process group
    } catch {
      try {
        child.kill(sig);
      } catch {
        /* already gone */
      }
    }
  };
  await new Promise<void>((resolve) => {
    let done = false;
    const finish = () => {
      if (!done) {
        done = true;
        resolve();
      }
    };
    child.once("exit", finish);
    killAll(signal);
    setTimeout(() => {
      killAll("SIGKILL");
      finish();
    }, 3000);
  });
}

async function deployFresh(port: number): Promise<Address> {
  const chain = localChain(port);
  const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(port)) });
  const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(port)) }) as PublicClient;
  const hash = await wallet.deployContract({
    abi: artifact.abi, // full ABI incl. constructor, so the recorder address arg is encoded
    bytecode: artifact.bytecode,
    args: [hd0.address],
  });
  const rcpt = await pub.waitForTransactionReceipt({ hash });
  if (!rcpt.contractAddress) throw new Error("deploy produced no contract address");
  return rcpt.contractAddress;
}

// The number of transactions the recorder's signer (hd0) has sent — i.e. its current nonce.
async function signerNonce(port: number): Promise<number> {
  const pub = createPublicClient({ chain: localChain(port), transport: viemHttp(rpcUrl(port)) }) as PublicClient;
  return pub.getTransactionCount({ address: hd0.address });
}

async function sealedCount(port: number, address: Address): Promise<number> {
  const pub = createPublicClient({ chain: localChain(port), transport: viemHttp(rpcUrl(port)) }) as PublicClient;
  const logs = await pub.getContractEvents({
    address,
    abi: STRATEGY_RECORD_ABI,
    eventName: "Sealed",
    fromBlock: 0n,
    toBlock: "latest",
  });
  return logs.length;
}

// Every server, proxy and child spawned by a test registers its teardown here, so a failing
// assertion can never leak an open handle and hang the test runner.
const cleanups: Array<() => Promise<void> | void> = [];

// --- Fake Kyros records server -----------------------------------------------------------------
type AckBody = { deployment_id: string; round_id?: number; tx_hash: string };
type AckDecision = "accept" | "refuse" | "hang";

function createFakeServer() {
  let pending: Record<string, unknown>[] = [];
  const acks: AckBody[] = [];
  const tokenHeaders: Array<string | undefined> = []; // x-recorder-token of every recorder request
  let ackHook: ((body: AckBody) => AckDecision) | null = null;

  const server = http.createServer((req, res) => {
    if (req.method === "GET" && req.url === "/api/records/pending") {
      tokenHeaders.push(req.headers["x-recorder-token"] as string | undefined);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(pending));
      return;
    }
    if (req.method === "POST" && req.url === "/api/records/ack") {
      tokenHeaders.push(req.headers["x-recorder-token"] as string | undefined);
      let data = "";
      req.on("data", (c) => (data += c));
      req.on("end", () => {
        const body = JSON.parse(data || "{}") as AckBody;
        const decision = ackHook ? ackHook(body) : "accept";
        if (decision === "hang") return; // never respond: simulates a crash before the ack lands
        if (decision === "refuse") {
          res.writeHead(500);
          res.end();
          return;
        }
        acks.push(body);
        pending = pending.filter(
          (p) => !(p.deployment_id === body.deployment_id && (p.round_id ?? null) === (body.round_id ?? null)),
        );
        res.writeHead(204);
        res.end();
      });
      return;
    }
    res.writeHead(404);
    res.end();
  });

  let port = 0;
  return {
    async listen(): Promise<string> {
      port = await freePort();
      await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
      cleanups.push(() => new Promise<void>((resolve) => server.close(() => resolve())));
      return `http://127.0.0.1:${port}`;
    },
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
    acks,
    tokenHeaders,
    setPending: (items: Record<string, unknown>[]) => (pending = items),
    getPending: () => pending,
    setAckHook: (h: ((body: AckBody) => AckDecision) | null) => (ackHook = h),
  };
}

// --- A down/up RPC proxy, for the retry test ---------------------------------------------------
function createProxy(targetPort: number) {
  let down = false;
  const server = http.createServer((req, res) => {
    if (down) {
      res.writeHead(502);
      res.end("down");
      return;
    }
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", async () => {
      try {
        const r = await fetch(rpcUrl(targetPort), {
          method: req.method,
          headers: { "content-type": "application/json" },
          body: body || undefined,
        });
        const text = await r.text();
        res.writeHead(r.status, { "content-type": "application/json" });
        res.end(text);
      } catch {
        res.writeHead(502);
        res.end("err");
      }
    });
  });
  let port = 0;
  return {
    async listen(): Promise<number> {
      port = await freePort();
      await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
      cleanups.push(() => new Promise<void>((resolve) => server.close(() => resolve())));
      return port;
    },
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
    setDown: (v: boolean) => (down = v),
  };
}

// --- An RPC proxy that fails its next N requests, then passes through --------------------------
// Unlike createProxy's on/off switch, this fails a bounded number of requests so a test can arm it
// *after* createContext's chain-id check and make exactly the first K attempts fail.
function countingProxy(targetPort: number) {
  let failuresLeft = 0;
  const server = http.createServer((req, res) => {
    if (failuresLeft > 0) {
      failuresLeft -= 1;
      res.writeHead(502);
      res.end("down");
      return;
    }
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", async () => {
      try {
        const r = await fetch(rpcUrl(targetPort), {
          method: req.method,
          headers: { "content-type": "application/json" },
          body: body || undefined,
        });
        const text = await r.text();
        res.writeHead(r.status, { "content-type": "application/json" });
        res.end(text);
      } catch {
        res.writeHead(502);
        res.end("err");
      }
    });
  });
  let port = 0;
  return {
    async listen(): Promise<number> {
      port = await freePort();
      await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
      cleanups.push(() => new Promise<void>((resolve) => server.close(() => resolve())));
      return port;
    },
    failNext: (n: number) => (failuresLeft = n),
  };
}

// Deploy a fresh StrategyRecord with an arbitrary recorder address (deployFresh uses hd0).
async function deployWithRecorder(port: number, recorder: Address): Promise<Address> {
  const chain = localChain(port);
  const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(port)) });
  const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(port)) }) as PublicClient;
  const hash = await wallet.deployContract({ abi: artifact.abi, bytecode: artifact.bytecode, args: [recorder] });
  const rcpt = await pub.waitForTransactionReceipt({ hash });
  if (!rcpt.contractAddress) throw new Error("deploy produced no contract address");
  return rcpt.contractAddress;
}

// Count on-chain writes by function name by wrapping the context's walletClient.writeContract.
function spyWrites(ctx: RecorderContext, fn: string): { count: number } {
  const state = { count: 0 };
  const client = ctx.clients.walletClient as unknown as { writeContract: (a: { functionName?: string }) => Promise<Hex> };
  const real = client.writeContract.bind(client);
  client.writeContract = (a: { functionName?: string }) => {
    if (a.functionName === fn) state.count += 1;
    return real(a);
  };
  return state;
}

// Record every getContractEvents call's raw block bounds, by wrapping the public client. `to` is
// left as viem receives it ("latest" for an unbounded scan), so a test can assert that no call runs
// to head in one shot.
type Bound = bigint | string | undefined;
function spyGetEvents(ctx: RecorderContext): Array<{ from: Bound; to: Bound }> {
  const calls: Array<{ from: Bound; to: Bound }> = [];
  const client = ctx.clients.publicClient as unknown as {
    getContractEvents: (a: { fromBlock?: Bound; toBlock?: Bound }) => Promise<unknown[]>;
  };
  const real = client.getContractEvents.bind(client);
  client.getContractEvents = async (a: { fromBlock?: Bound; toBlock?: Bound }) => {
    calls.push({ from: a.fromBlock, to: a.toBlock });
    return real(a);
  };
  return calls;
}

// --- A log collector, so tests can assert on recorder output -----------------------------------
function collector(): Logger & { lines: string[] } {
  const lines: string[] = [];
  return {
    lines,
    info: (m) => lines.push(m),
    warn: (m) => lines.push(m),
    error: (m) => lines.push(m),
  };
}

const TOKEN = "s3cr3t-recorder-token-value"; // the shared recorder token used across the tests

function baseEnv(apiUrl: string, port: number, contract: Address): NodeJS.ProcessEnv {
  return {
    KYROS_API_URL: apiUrl,
    ARC_RPC_URL: rpcUrl(port),
    STRATEGY_RECORD_ADDRESS: contract,
    RECORDER_PRIVATE_KEY: RECORDER_KEY,
    RECORDER_TOKEN: TOKEN,
    RECORDER_ALLOW_LOCAL: "1",
    RECORDER_POLL_MS: "80",
    RECORDER_BACKOFF_BASE_MS: "40",
    RECORDER_BACKOFF_MAX_MS: "200",
  };
}

const DID = keccak256(toHex("deployment:recorder-it:1"));
const PROGRAM = keccak256(toHex("program"));
const JOURNAL = keccak256(toHex("journal"));

function openItem(): Record<string, unknown> {
  return { kind: "open", deployment_id: DID, program_hash: PROGRAM, budget_cents: 25000 };
}
const DID2 = keccak256(toHex("deployment:recorder-it:2"));
/** An open for a second deployment: the "next item" that a failed open must not block. */
function otherOpenItem(): Record<string, unknown> {
  return { kind: "open", deployment_id: DID2, program_hash: PROGRAM, budget_cents: 25000 };
}
function sealItem(round: number): Record<string, unknown> {
  return {
    kind: "seal",
    deployment_id: DID,
    round_id: round,
    won: round % 2 === 0,
    pnl_cents: round % 2 === 0 ? 100 * round : -100 * round,
    journal_hash: JOURNAL,
  };
}

// ===============================================================================================
let nodePort = 0;
let node: ChildProcess | undefined;

describe("recorder", () => {
  before(async () => {
    nodePort = await freePort();
    node = await startNode(nodePort);
  });
  after(async () => {
    for (const c of cleanups.splice(0)) {
      try {
        await c();
      } catch {
        /* best effort */
      }
    }
    await stopChild(node, "SIGKILL");
  });

  it("ten seals in → ten acks out, each with a real tx hash, in order", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem(), ...Array.from({ length: 10 }, (_, i) => sealItem(i))]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    await waitUntil(async () => {
      await pollOnce(ctx);
      return srv.getPending().length === 0;
    }, 30000);

    // One open ack (no round_id) + ten seal acks (round_id 0..9), in order.
    const sealAcks = srv.acks.filter((a) => a.round_id !== undefined);
    const openAcks = srv.acks.filter((a) => a.round_id === undefined);
    assert.equal(openAcks.length, 1, "open acked exactly once");
    assert.equal(sealAcks.length, 10, "ten seal acks");
    assert.deepEqual(
      sealAcks.map((a) => a.round_id),
      [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
      "seal acks arrived in order",
    );

    // Every ack's tx hash is a real on-chain transaction.
    const pub = createPublicClient({ chain: localChain(nodePort), transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    for (const a of srv.acks) {
      assert.match(a.tx_hash, /^0x[0-9a-fA-F]{64}$/, "tx hash is 32 bytes hex");
      const rcpt = await pub.getTransactionReceipt({ hash: a.tx_hash as Hex });
      assert.equal(rcpt.status, "success");
    }
    assert.equal(await sealedCount(nodePort, contract), 10, "exactly ten Sealed events");

    await srv.close();
  });

  it("crash after the 5th send and before its ack, then restart → all ten acked, none sent twice", { timeout: 90000 }, async () => {
    const contract = await deployFresh(nodePort);
    const nonceBefore = await signerNonce(nodePort); // after the deploy, before any recorder run
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem(), ...Array.from({ length: 10 }, (_, i) => sealItem(i))]);

    // Hang (never answer) the 5th seal ack, so from the system's view it is sent-but-not-acked.
    let acceptedSealAcks = 0;
    let crashSignaled = false;
    srv.setAckHook((body) => {
      if (body.round_id === undefined) return "accept";
      if (acceptedSealAcks === 4) {
        crashSignaled = true;
        return "hang";
      }
      acceptedSealAcks += 1;
      return "accept";
    });

    const env1 = baseEnv(apiUrl, nodePort, contract);
    let out1 = "";
    const child1 = spawn("node", [recorderEntry.pathname], { cwd: chainDir, env: { ...process.env, ...env1 }, detached: true });
    cleanups.push(() => stopChild(child1, "SIGKILL"));
    child1.stdout.on("data", (d) => (out1 += d.toString()));
    child1.stderr.on("data", (d) => (out1 += d.toString()));

    await waitUntil(() => crashSignaled, 30000);
    await stopChild(child1, "SIGKILL");

    // At this point: seals 0..4 sent+confirmed; seal 4 (the 5th) on chain but its ack never landed.
    assert.ok((await sealedCount(nodePort, contract)) >= 5, "at least five seals already on chain");
    assert.ok(srv.getPending().some((p) => p.round_id === 4), "the 5th seal is still pending");

    // Restart with acks flowing normally.
    srv.setAckHook(null);
    let out2 = "";
    const child2 = spawn("node", [recorderEntry.pathname], { cwd: chainDir, env: { ...process.env, ...env1 }, detached: true });
    cleanups.push(() => stopChild(child2, "SIGKILL"));
    child2.stdout.on("data", (d) => (out2 += d.toString()));
    child2.stderr.on("data", (d) => (out2 += d.toString()));

    await waitUntil(() => srv.getPending().length === 0, 30000);
    await stopChild(child2, "SIGINT");

    const sealAcks = srv.acks.filter((a) => a.round_id !== undefined).map((a) => a.round_id);
    assert.deepEqual(new Set(sealAcks), new Set([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), "all ten seals acked");
    assert.equal(await sealedCount(nodePort, contract), 10, "exactly ten Sealed events — none sent twice");
    assert.match(out2, /already on chain, acking without sending/, "restart re-acked without re-sending");

    // The real idempotency check: the signer sent exactly one transaction per distinct record
    // (1 open + 10 seals = 11) across both runs. Unlike the Sealed-event count, this *can* fail if
    // the recorder re-sends an already-recorded item (a duplicate send bumps the nonce even when the
    // contract de-duplicates the event). F4.
    const nonceAfter = await signerNonce(nodePort);
    assert.equal(nonceAfter - nonceBefore, 11, "the signer sent exactly one tx per distinct record (no re-sends)");

    await srv.close();
  });

  it("a malformed pending item is skipped and logged, the rest proceed", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([
      openItem(),
      { kind: "seal", deployment_id: "0xnothex", round_id: 0, won: true, pnl_cents: 1, journal_hash: JOURNAL },
      sealItem(1),
    ]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    await waitUntil(async () => {
      await pollOnce(ctx);
      return srv.getPending().length <= 1; // the malformed one can never be acked, so it lingers
    }, 30000);

    assert.ok(log.lines.some((l) => /skipping malformed record/.test(l)), "malformed item logged");
    // The valid open and seal(1) were acked; the malformed seal(0) was never acked.
    const acked = srv.acks.map((a) => ({ did: a.deployment_id, round: a.round_id }));
    assert.ok(acked.some((a) => a.round === undefined), "open acked");
    assert.ok(acked.some((a) => a.round === 1), "the valid seal acked");
    assert.ok(!acked.some((a) => a.round === 0), "the malformed seal was never acked");
    assert.equal(await sealedCount(nodePort, contract), 1, "only the valid seal reached the chain");

    await srv.close();
  });

  it("a failed send (RPC down, then up) is retried with the same arguments", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem()]);

    const proxy = createProxy(nodePort);
    const proxyPort = await proxy.listen();

    // Point the recorder at the proxy; verify the chain through it while it is up.
    const env = baseEnv(apiUrl, proxyPort, contract);
    const config = loadConfig(env);
    const log = collector();
    const ctx = await createContext(config, log);

    proxy.setDown(true);
    const running = pollOnce(ctx); // blocks in the retry loop while the proxy is down
    await delay(300);
    proxy.setDown(false);
    await running;

    assert.ok(log.lines.some((l) => /send failed .*retrying/.test(l)), "a retry was logged");
    assert.equal(srv.acks.length, 1, "the record was eventually acked");
    assert.match(srv.acks[0].tx_hash, /^0x[0-9a-fA-F]{64}$/);

    // Same arguments landed on chain: the Opened event matches the input.
    const pub = createPublicClient({ chain: localChain(nodePort), transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const logs = await pub.getContractEvents({
      address: contract,
      abi: STRATEGY_RECORD_ABI,
      eventName: "Opened",
      fromBlock: 0n,
      toBlock: "latest",
    });
    assert.equal(logs.length, 1);
    assert.equal(logs[0].args.deploymentId, DID);
    assert.equal(logs[0].args.programHash, PROGRAM);
    assert.equal(logs[0].args.budgetCents, 25000n);

    await proxy.close();
    await srv.close();
  });

  it("refuses to start on the wrong chain id", { timeout: 30000 }, async () => {
    const contract = await deployFresh(nodePort);
    // No RECORDER_ALLOW_LOCAL: the local chain 31337 is not Arc Testnet, so it must refuse.
    const env = { ...baseEnv("http://127.0.0.1:1/unused", nodePort, contract) };
    delete env.RECORDER_ALLOW_LOCAL;

    let out = "";
    const child = spawn("node", [recorderEntry.pathname], { cwd: chainDir, env: { ...process.env, ...env }, detached: true });
    cleanups.push(() => stopChild(child, "SIGKILL"));
    child.stdout.on("data", (d) => (out += d.toString()));
    child.stderr.on("data", (d) => (out += d.toString()));
    const code = await new Promise<number>((resolve) => child.once("exit", (c) => resolve(c ?? -1)));

    assert.notEqual(code, 0, "process exited non-zero");
    assert.match(out, /refusing to run: connected to chain 31337/, "it said why it refused");
    assert.ok(!out.includes(RECORDER_KEY), "the key never appears in output");
  });

  it("validateRecord accepts good records and rejects malformed ones (unit)", () => {
    const good = validateRecord(openItem()) as Extract<StrategyRecordItem, { kind: "open" }>;
    assert.equal(good.kind, "open");
    assert.equal(good.budgetCents, 25000n);

    assert.throws(() => validateRecord({ kind: "open", deployment_id: "0x12" }), ValidationError);
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: -1, won: true, pnl_cents: 0, journal_hash: JOURNAL }),
      ValidationError,
    );
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: 0, won: "yes", pnl_cents: 0, journal_hash: JOURNAL }),
      ValidationError,
    );
    assert.throws(() => validateRecord({ kind: "frob", deployment_id: DID }), ValidationError);
  });

  it("verifyChainId refuses the local chain without the opt-in (unit)", { timeout: 30000 }, async () => {
    const config = loadConfig({ ...baseEnv("http://127.0.0.1:1", nodePort, await deployFresh(nodePort)), RECORDER_ALLOW_LOCAL: "" });
    const { publicClient } = createClients(config);
    await assert.rejects(verifyChainId(publicClient as PublicClient, config), /connected to chain 31337/);
  });

  it("validateRecord rejects unsafe-integer numbers and out-of-range values, accepts integer strings (unit, F6/G2)", () => {
    // A JSON number above 2^53 silently loses precision as a double, so it must be rejected.
    assert.throws(() => validateRecord({ ...openItem(), budget_cents: 9007199254740993 }), ValidationError);
    // budget_cents of the same magnitude as an exact decimal string is accepted: it is a uint64
    // written on chain as a bigint and is never round-tripped through a JS number, so it is exact.
    const ok = validateRecord({ ...openItem(), budget_cents: "9007199254740993" }) as Extract<StrategyRecordItem, { kind: "open" }>;
    assert.equal(ok.budgetCents, 9007199254740993n);
    // round_id is different: ackRecord sends Number(round_id) back to the server, so a value above
    // Number.MAX_SAFE_INTEGER cannot round-trip and is malformed in ANY form — string or number (G2).
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: "9007199254740993", won: true, pnl_cents: 0, journal_hash: JOURNAL }),
      ValidationError,
      "round_id string above 2^53-1 is rejected",
    );
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: 9007199254740993, won: true, pnl_cents: 0, journal_hash: JOURNAL }),
      ValidationError,
      "round_id number above 2^53-1 is rejected",
    );
    // The maximum safe integer is the largest accepted round_id and round-trips exactly.
    const okRound = validateRecord({ kind: "seal", deployment_id: DID, round_id: "9007199254740991", won: true, pnl_cents: 0, journal_hash: JOURNAL }) as Extract<StrategyRecordItem, { kind: "seal" }>;
    assert.equal(okRound.roundId, 9007199254740991n);
    // Range enforcement (both as numbers and as strings).
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: -1, won: true, pnl_cents: 0, journal_hash: JOURNAL }),
      ValidationError,
      "round_id -1 is below the uint64 floor",
    );
    assert.throws(
      () => validateRecord({ ...openItem(), budget_cents: "18446744073709551616" }), // 2^64
      ValidationError,
      "budget_cents 2^64 is above the uint64 ceiling",
    );
    assert.throws(
      () => validateRecord({ kind: "seal", deployment_id: DID, round_id: 0, won: true, pnl_cents: "9223372036854775808", journal_hash: JOURNAL }), // 2^63
      ValidationError,
      "pnl_cents 2^63 is above the int64 ceiling",
    );
    assert.throws(() => validateRecord({ ...openItem(), budget_cents: 1.5 }), ValidationError, "a non-integer is rejected");
  });

  it("ackRecord sends round_id: 9007199254740991 exactly for a max-safe round (G2)", { timeout: 30000 }, async () => {
    let received: AckBody | null = null;
    const server = http.createServer((req, res) => {
      let data = "";
      req.on("data", (c) => (data += c));
      req.on("end", () => {
        received = JSON.parse(data || "{}") as AckBody;
        res.writeHead(204);
        res.end();
      });
    });
    const port = await freePort();
    await new Promise<void>((r) => server.listen(port, "127.0.0.1", () => r()));
    cleanups.push(() => new Promise<void>((r) => server.close(() => r())));
    const cfg = { apiUrl: `http://127.0.0.1:${port}`, httpTimeoutMs: 10000 } as unknown as Config;

    const rec = validateRecord({ kind: "seal", deployment_id: DID, round_id: "9007199254740991", won: true, pnl_cents: 0, journal_hash: JOURNAL });
    await ackRecord(cfg, rec, ("0x" + "ab".repeat(32)) as Hex);

    assert.ok(received, "the ack reached the server");
    assert.equal((received as AckBody).round_id, 9007199254740991, "round_id is sent exactly, with no precision loss");
    await new Promise<void>((r) => server.close(() => r()));
  });

  it("ackRecord accepts any 2xx and rejects non-2xx (unit, F7)", { timeout: 30000 }, async () => {
    let status = 204;
    const server = http.createServer((_req, res) => {
      res.writeHead(status);
      res.end();
    });
    const port = await freePort();
    await new Promise<void>((r) => server.listen(port, "127.0.0.1", () => r()));
    cleanups.push(() => new Promise<void>((r) => server.close(() => r())));
    const cfg = { apiUrl: `http://127.0.0.1:${port}`, httpTimeoutMs: 10000 } as unknown as Config;
    const rec = validateRecord(openItem());
    const hash = (n: string) => ("0x" + n.repeat(32)) as Hex;

    status = 204;
    await ackRecord(cfg, rec, hash("11")); // resolves
    status = 200;
    await ackRecord(cfg, rec, hash("22")); // resolves — any 2xx counts as acked
    status = 500;
    await assert.rejects(ackRecord(cfg, rec, hash("33")), /\/api\/records\/ack → 500/);

    await new Promise<void>((r) => server.close(() => r()));
  });

  it("a reverting item is attempted once, left pending, and does not block the next item (F1-1)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    // A seal whose deployment was never opened reverts NotOpened. Opens are tried first, so the
    // reverting seal sits between another deployment's open and that deployment's seal.
    srv.setPending([sealItem(0), otherOpenItem(), { ...sealItem(1), deployment_id: DID2 }]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);
    const seals = spyWrites(ctx, "seal");

    await pollOnce(ctx); // ONE poll

    assert.ok(srv.acks.some((a) => a.deployment_id === DID2 && a.round_id === undefined), "the other deployment's open was recorded and acked");
    assert.ok(srv.acks.some((a) => a.deployment_id === DID2 && a.round_id === 1), "the item after the reverting seal was recorded and acked in the same poll");
    assert.ok(!srv.acks.some((a) => a.deployment_id === DID), "the reverting seal was not acked");
    assert.equal(seals.count, 2, "the reverting seal was attempted exactly once (not retried), the other seal once");
    assert.ok(srv.getPending().some((p) => p.deployment_id === DID && p.round_id === 0), "the reverting seal is left pending on the server");

    // A SECOND poll, now that its open is pending too, records the open and then the seal.
    srv.setPending([...srv.getPending(), openItem()]);
    await pollOnce(ctx);
    assert.ok(srv.acks.some((a) => a.deployment_id === DID && a.round_id === 0), "the second poll recorded and acked the seal");
    assert.equal(await sealedCount(nodePort, contract), 2, "exactly two Sealed events");

    await srv.close();
  });

  it("a signer that is not the recorder gives up each open, holds the seal behind it, and the poll still returns (F1-2)", { timeout: 60000 }, async () => {
    const otherRecorder = mnemonicToAccount(MNEMONIC, { addressIndex: 1 }).address;
    const contract = await deployWithRecorder(nodePort, otherRecorder);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem(), sealItem(0), otherOpenItem()]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);
    const opens = spyWrites(ctx, "open");
    const seals = spyWrites(ctx, "seal");

    await pollOnce(ctx); // must return, not hang

    assert.equal(srv.acks.length, 0, "nothing was acked");
    assert.equal(opens.count, 2, "each deployment's open attempted exactly once");
    assert.equal(seals.count, 0, "the seal behind the failed open was held, not attempted");
    assert.equal(srv.getPending().length, 3, "every item left pending");

    await srv.close();
  });

  it("retries a transient RPC failure, then gives up after maxAttempts and still processes the next item (F1-3)", { timeout: 90000 }, async () => {
    // Phase A: first two attempts fail, the third succeeds.
    {
      const contract = await deployFresh(nodePort);
      const srv = createFakeServer();
      const apiUrl = await srv.listen();
      srv.setPending([openItem()]);
      const proxy = countingProxy(nodePort);
      const proxyPort = await proxy.listen();
      const config = loadConfig({ ...baseEnv(apiUrl, proxyPort, contract), RECORDER_MAX_ATTEMPTS: "5" });
      const log = collector();
      const ctx = await createContext(config, log); // chain-id check runs while the proxy is up
      proxy.failNext(2); // fail the next two requests (attempts 1 and 2 at isPresent)

      await pollOnce(ctx);

      assert.equal(srv.acks.length, 1, "the record was acked after recovery");
      assert.equal(await sealedCount(nodePort, contract), 0);
      assert.ok(log.lines.filter((l) => /retrying/.test(l)).length >= 2, "at least two retries were logged");
      await srv.close();
    }

    // Phase B: RPC down throughout → give up after maxAttempts, still process the next item.
    {
      const contract = await deployFresh(nodePort);
      const srv = createFakeServer();
      const apiUrl = await srv.listen();
      srv.setPending([openItem(), otherOpenItem()]);
      const proxy = countingProxy(nodePort);
      const proxyPort = await proxy.listen();
      const config = loadConfig({ ...baseEnv(apiUrl, proxyPort, contract), RECORDER_MAX_ATTEMPTS: "2" });
      const log = collector();
      const ctx = await createContext(config, log); // runs while the proxy is up
      proxy.failNext(1_000_000); // down for the rest of the poll

      await pollOnce(ctx); // must return

      assert.equal(srv.acks.length, 0, "nothing acked while the RPC is down");
      assert.equal(
        log.lines.filter((l) => /giving up on open .* after 2 attempt/.test(l)).length,
        2,
        "the first open was given up after maxAttempts and the next item (another deployment's open) was still processed",
      );
    }
  });

  it("stopRequested halts the item loop before the next item (F1-4)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem(), sealItem(0)]);
    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    // Flip the stop flag the moment the first item's write is issued, so the loop must stop before item 2.
    let stop = false;
    ctx.stopRequested = () => stop;
    const client = ctx.clients.walletClient as unknown as { writeContract: (a: { functionName?: string }) => Promise<Hex> };
    const realWrite = client.writeContract.bind(client);
    const seals = { count: 0 };
    client.writeContract = (a: { functionName?: string }) => {
      if (a.functionName === "seal") seals.count += 1;
      const p = realWrite(a);
      if (a.functionName === "open") stop = true;
      return p;
    };

    await pollOnce(ctx);

    assert.ok(srv.acks.some((x) => x.round_id === undefined), "the first item (open) was recorded");
    assert.ok(!srv.acks.some((x) => x.round_id === 0), "the second item (seal) was not reached");
    assert.equal(seals.count, 0, "the seal was never attempted");
    await srv.close();
  });

  it("a transaction mined but reverted is never acked (F2)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem()]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    // Stub the receipt to report a revert, and report the record as absent, so the item is unsendable.
    const pub = ctx.clients.publicClient as unknown as {
      waitForTransactionReceipt: (a: unknown) => Promise<{ status: string }>;
      readContract: (a: unknown) => Promise<unknown>;
    };
    pub.waitForTransactionReceipt = async () => ({ status: "reverted" });
    pub.readContract = async () => false; // isPresent → false, both before the send and on the re-check

    await pollOnce(ctx);

    assert.equal(srv.acks.length, 0, "a mined-but-reverted transaction is never acked");
    assert.ok(log.lines.some((l) => /mined but reverted/.test(l)), "it logged the revert");
    await srv.close();
  });

  it("findTxHash scans the log in chunks no wider than logChunk, newest first, and still finds the hash (F3)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();

    // Write the open on chain directly, then mine several more blocks so the event sits behind head.
    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, PROGRAM, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });
    for (let i = 0; i < 6; i++) {
      const h = await wallet.sendTransaction({ account: hd0, chain, to: hd0.address, value: 0n });
      await pub.waitForTransactionReceipt({ hash: h });
    }

    // The same open is pending; with a tiny chunk the recorder must still find and ack it.
    srv.setPending([openItem()]);
    const config = loadConfig({ ...baseEnv(apiUrl, nodePort, contract), RECORDER_LOG_CHUNK: "2", RECORDER_FROM_BLOCK: "0" });
    const log = collector();
    const ctx = await createContext(config, log);
    const calls = spyGetEvents(ctx);

    await pollOnce(ctx);

    assert.ok(srv.acks.some((a) => a.round_id === undefined), "the already-present open was found and acked");
    assert.ok(calls.length > 1, "the scan was split into more than one chunk");
    for (const c of calls) {
      assert.notEqual(c.to, "latest", "no call scans all the way to head in one shot");
      assert.ok(
        typeof c.from === "bigint" && typeof c.to === "bigint" && c.to - c.from + 1n <= 2n,
        `a getContractEvents call spanned ${typeof c.to === "bigint" && typeof c.from === "bigint" ? c.to - c.from + 1n : "?"} blocks (> logChunk)`,
      );
    }
    await srv.close();
  });

  it("findTxHash refuses to ack when the on-chain record differs from the pending item (F5)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();

    // On chain: open, then seal round 7 with journal hash A (won=true, pnl=42).
    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const A = keccak256(toHex("journal-A"));
    const B = keccak256(toHex("journal-B"));
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, PROGRAM, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });
    const sh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "seal", args: [DID, 7n, true, 42n, A] });
    await pub.waitForTransactionReceipt({ hash: sh });

    // The pending item agrees on won/pnl but names a DIFFERENT journal hash (B).
    srv.setPending([{ kind: "seal", deployment_id: DID, round_id: 7, won: true, pnl_cents: 42, journal_hash: B }]);
    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    await pollOnce(ctx);

    assert.ok(!srv.acks.some((a) => a.round_id === 7), "a differing on-chain record is not acked");
    assert.ok(log.lines.some((l) => /on-chain record differs.*journalHash/.test(l)), "the differing field (journalHash) is named");
    await srv.close();
  });

  it("loadConfig requires RECORDER_TOKEN (unit, F9)", () => {
    const base = baseEnv("http://127.0.0.1:1", nodePort, hd0.address);
    assert.doesNotThrow(() => loadConfig(base), "a configured token is accepted");
    const { RECORDER_TOKEN: _omit, ...noToken } = base;
    void _omit;
    assert.throws(() => loadConfig(noToken), /RECORDER_TOKEN/, "a missing token is rejected");
    assert.throws(() => loadConfig({ ...base, RECORDER_TOKEN: "" }), /RECORDER_TOKEN/, "an empty token is rejected");
  });

  it("every recorder request carries the x-recorder-token header (F9)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem(), ...Array.from({ length: 10 }, (_, i) => sealItem(i))]);

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    await waitUntil(async () => {
      await pollOnce(ctx);
      return srv.getPending().length === 0;
    }, 30000);

    // At least one GET /pending plus eleven POST /ack (1 open + 10 seals); every one carried the token.
    assert.ok(srv.tokenHeaders.length >= 12, `expected >= 12 recorder requests, saw ${srv.tokenHeaders.length}`);
    assert.ok(srv.tokenHeaders.every((t) => t === TOKEN), "every recorder request carried the configured token");
    await srv.close();
  });

  it("a 401 from pending is a token-refused poll error that logs without leaking the token (F9)", { timeout: 30000 }, async () => {
    const contract = await deployFresh(nodePort);
    const server = http.createServer((_req, res) => {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "recorder_unauthorized" }));
    });
    const port = await freePort();
    await new Promise<void>((r) => server.listen(port, "127.0.0.1", () => r()));
    cleanups.push(() => new Promise<void>((r) => server.close(() => r())));
    const apiUrl = `http://127.0.0.1:${port}`;

    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    // The function the loop calls rejects with a token-refused message (not an UnsendableRecordError).
    await assert.rejects(fetchPending(config), /refused the recorder's token/);

    // The loop itself logs it, sends no transaction, and never leaks the token value.
    const log = collector();
    const ctx = await createContext(config, log);
    const writes = spyWrites(ctx, "open");
    await pollOnce(ctx);
    assert.equal(writes.count, 0, "no transaction was sent");
    assert.ok(log.lines.some((l) => /refused the recorder's token/.test(l)), "the refusal was logged");
    assert.ok(!log.lines.some((l) => l.includes(TOKEN)), "the token value never appears in any log line");

    await new Promise<void>((r) => server.close(() => r()));
  });

  // --- G1: no URL path or query may reach a log line ------------------------------------------
  it("redactUrls keeps only a URL's origin, dropping user-info, path and query (unit, G1)", () => {
    const out = redactUrls("x URL: http://127.0.0.1:1/v2/FAKEAPIKEY123?token=SECRET y");
    assert.ok(out.includes("http://127.0.0.1:1"), "the origin is kept");
    assert.ok(!out.includes("FAKEAPIKEY123"), "the path (api key) is dropped");
    assert.ok(!out.includes("SECRET"), "the query (secret) is dropped");

    const out2 = redactUrls("boom https://user:pw@host.example/v2/KEY");
    assert.ok(out2.includes("https://host.example"), "the origin is kept");
    assert.ok(!out2.includes("user") && !out2.includes("pw") && !out2.includes("KEY"), "user-info and path are dropped");

    // ws/wss are handled too, and an unparseable URL becomes <url>.
    assert.equal(redactUrls("see ws://h:5/stream?k=1"), "see ws://h:5");
    assert.ok(redactUrls("bad http://%%% end").includes("<url>"), "an unparseable URL is replaced with <url>");
  });

  it("no log line carries the RPC key when the RPC is unreachable (G1)", { timeout: 30000 }, async () => {
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setPending([openItem()]);

    // Nothing listens on port 1; the viem error embeds the full RPC URL (key and all).
    const rpcWithKey = "http://127.0.0.1:1/v2/FAKEAPIKEY123";
    const config = loadConfig({
      ...baseEnv(apiUrl, nodePort, hd0.address),
      ARC_RPC_URL: rpcWithKey,
      RECORDER_MAX_ATTEMPTS: "2",
      RECORDER_BACKOFF_BASE_MS: "10",
      RECORDER_BACKOFF_MAX_MS: "20",
      RECORDER_HTTP_TIMEOUT_MS: "200",
    });
    const clients = createClients(config);
    const log = collector();
    // Build the context by hand: createContext would call verifyChainId, which cannot reach the
    // dead RPC. The chain object is only used for writes, which never land here.
    const ctx: RecorderContext = {
      config,
      clients: clients as unknown as RecorderContext["clients"],
      chain: localChain(nodePort),
      log,
      stopRequested: () => false,
    };

    await pollOnce(ctx);

    assert.ok(log.lines.length > 0, "something was logged");
    assert.ok(log.lines.some((l) => /retrying|giving up|could not record/.test(l)), "at least one warn line was logged");
    assert.ok(!log.lines.some((l) => l.includes("FAKEAPIKEY123")), "no log line carries the RPC key");

    await srv.close();
  });

  // --- G4: nothing waits without a limit; a stop must not wait out a long sleep ---------------
  it("fetchPending aborts a hung request within httpTimeoutMs (G4a)", { timeout: 15000 }, async () => {
    // A server that accepts the connection and never answers.
    const sockets = new Set<net.Socket>();
    const server = http.createServer(() => {
      /* never respond */
    });
    server.on("connection", (s) => {
      sockets.add(s);
      s.on("close", () => sockets.delete(s));
    });
    const port = await freePort();
    await new Promise<void>((r) => server.listen(port, "127.0.0.1", () => r()));
    const shut = () =>
      new Promise<void>((r) => {
        for (const s of sockets) s.destroy();
        server.close(() => r());
      });
    cleanups.push(shut);

    const config = loadConfig({ ...baseEnv(`http://127.0.0.1:${port}`, nodePort, hd0.address), RECORDER_HTTP_TIMEOUT_MS: "200" });
    const start = Date.now();
    await assert.rejects(fetchPending(config), (e: Error) => /timeout|abort/i.test(`${e.name} ${e.message}`));
    const elapsed = Date.now() - start;
    assert.ok(elapsed < 2000, `fetchPending rejected in ${elapsed}ms (expected ~httpTimeoutMs, < 2s)`);

    await shut();
  });

  it("a stop during a long backoff makes resolveTxHash reject in under 2s (G4b)", { timeout: 15000 }, async () => {
    const contract = await deployFresh(nodePort);
    const config = loadConfig({
      ...baseEnv("http://127.0.0.1:1/unused", nodePort, contract),
      RECORDER_MAX_ATTEMPTS: "5",
      RECORDER_BACKOFF_BASE_MS: "30000",
      RECORDER_BACKOFF_MAX_MS: "30000",
    });
    const log = collector();
    const ctx = await createContext(config, log);

    let stop = false;
    ctx.stopRequested = () => stop;
    // First (and only) attempt fails with a non-revert error; the backoff would otherwise be 30s.
    const pub = ctx.clients.publicClient as unknown as { readContract: (a: unknown) => Promise<unknown> };
    pub.readContract = async () => {
      throw new Error("transient RPC failure");
    };
    setTimeout(() => {
      stop = true;
    }, 100);

    const start = Date.now();
    await assert.rejects(resolveTxHash(ctx, validateRecord(openItem())), /transient RPC failure/);
    const elapsed = Date.now() - start;
    assert.ok(elapsed < 2000, `resolveTxHash rejected in ${elapsed}ms (expected < 2s despite a 30s backoff)`);
  });

  // --- G5: a refused open holds back that deployment's seals ----------------------------------
  it("a deployment whose on-chain open differs holds back its seals (G5)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();

    // On chain: open D with programHash A.
    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const A = keccak256(toHex("program-A"));
    const B = keccak256(toHex("program-B"));
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, A, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });

    const isSealedOnChain = (round: number) =>
      pub.readContract({ address: contract, abi: STRATEGY_RECORD_ABI, functionName: "isSealed", args: [DID, BigInt(round)] }) as Promise<boolean>;

    // Pending: open D with a DIFFERENT programHash B (same budget), then a seal for D.
    srv.setPending([{ kind: "open", deployment_id: DID, program_hash: B, budget_cents: 25000 }, sealItem(1)]);
    const config = loadConfig(baseEnv(apiUrl, nodePort, contract));
    const log = collector();
    const ctx = await createContext(config, log);

    await pollOnce(ctx);
    assert.equal(await isSealedOnChain(1), false, "poll 1: the seal was not written");
    assert.equal(srv.acks.length, 0, "poll 1: nothing was acked");
    assert.ok(log.lines.some((l) => /held|holding/.test(l)), "poll 1: it logged that the seal is held");

    // A second poll holds the same: the open still differs on chain, the seal is still held.
    await pollOnce(ctx);
    assert.equal(await isSealedOnChain(1), false, "poll 2: the seal was still not written");
    assert.equal(srv.acks.length, 0, "poll 2: still nothing acked");
    assert.equal(await sealedCount(nodePort, contract), 0, "no Sealed event was ever emitted for D");

    await srv.close();
  });

  // --- H: round-3 fixes to the hold-back and the stop ------------------------------------------
  /** Chain holds open(D, A); returns helpers and the two program hashes. */
  async function chainWithOpenA(contract: Address) {
    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const A = keccak256(toHex("program-A"));
    const B = keccak256(toHex("program-B"));
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, A, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });
    const isSealedOnChain = (round: number) =>
      pub.readContract({ address: contract, abi: STRATEGY_RECORD_ABI, functionName: "isSealed", args: [DID, BigInt(round)] }) as Promise<boolean>;
    return { A, B, isSealedOnChain };
  }

  it("an open that could not be checked this poll holds back its seals too (H1)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    const { B, isSealedOnChain } = await chainWithOpenA(contract);

    // Pending: a differing open, then a seal. The log read that would show the difference fails once.
    srv.setPending([{ kind: "open", deployment_id: DID, program_hash: B, budget_cents: 25000 }, sealItem(1)]);
    const config = loadConfig({ ...baseEnv(apiUrl, nodePort, contract), RECORDER_MAX_ATTEMPTS: "1" });
    const log = collector();
    const ctx = await createContext(config, log);
    const client = ctx.clients.publicClient as unknown as { getContractEvents: (a: unknown) => Promise<unknown[]> };
    const real = client.getContractEvents.bind(client);
    let failures = 1;
    client.getContractEvents = async (a: unknown) => {
      if (failures > 0) {
        failures -= 1;
        throw new Error("transient log read failure");
      }
      return real(a);
    };

    await pollOnce(ctx);
    assert.ok(log.lines.some((l) => /transient log read failure/.test(l)), "the open's check failed this poll");
    assert.equal(await isSealedOnChain(1), false, "the seal was not written behind an unchecked open");
    assert.equal(srv.acks.length, 0, "nothing was acked");

    await pollOnce(ctx);
    assert.equal(await isSealedOnChain(1), false, "poll 2: the open is seen to differ, the seal stays held");
    assert.equal(srv.acks.length, 0);
    await srv.close();
  });

  it("a malformed open holds back its deployment's seals (H5)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    const { isSealedOnChain } = await chainWithOpenA(contract);

    srv.setPending([{ kind: "open", deployment_id: DID, program_hash: "0xbad", budget_cents: 25000 }, sealItem(1)]);
    const log = collector();
    const ctx = await createContext(loadConfig(baseEnv(apiUrl, nodePort, contract)), log);
    await pollOnce(ctx);

    assert.ok(log.lines.some((l) => /malformed/.test(l)), "the open was reported as malformed");
    assert.equal(await isSealedOnChain(1), false, "the seal was not written behind an open that could not be read");
    assert.equal(srv.acks.length, 0, "nothing was acked");
    await srv.close();
  });

  it("opens are tried before seals whatever order the server lists them in (H6)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    const { B, isSealedOnChain } = await chainWithOpenA(contract);

    // The seal is listed first; its open (which differs from the one on chain) second.
    srv.setPending([sealItem(1), { kind: "open", deployment_id: DID, program_hash: B, budget_cents: 25000 }]);
    const ctx = await createContext(loadConfig(baseEnv(apiUrl, nodePort, contract)), collector());
    await pollOnce(ctx);

    assert.equal(await isSealedOnChain(1), false, "the seal was held although it was listed before its open");
    assert.equal(srv.acks.length, 0, "nothing was acked");
    await srv.close();
  });

  it("held seals go through once the pending open matches the chain (H2)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    const { A, B, isSealedOnChain } = await chainWithOpenA(contract);

    srv.setPending([{ kind: "open", deployment_id: DID, program_hash: B, budget_cents: 25000 }, sealItem(1)]);
    const ctx = await createContext(loadConfig(baseEnv(apiUrl, nodePort, contract)), collector());
    await pollOnce(ctx);
    assert.equal(await isSealedOnChain(1), false, "held while the open differs");

    // The server's open is corrected to what is on chain.
    srv.setPending([{ kind: "open", deployment_id: DID, program_hash: A, budget_cents: 25000 }, sealItem(1)]);
    await pollOnce(ctx);
    assert.equal(await isSealedOnChain(1), true, "the seal was written once the open matched");
    assert.deepEqual(
      srv.acks.map((a) => a.round_id ?? null),
      [null, 1],
      "the open was acked, then the seal",
    );
    await srv.close();
  });

  it("ackRecord aborts a hung request within httpTimeoutMs (H3)", { timeout: 15000 }, async () => {
    const srv = createFakeServer();
    const apiUrl = await srv.listen();
    srv.setAckHook(() => "hang");
    const config = loadConfig({ ...baseEnv(apiUrl, nodePort, hd0.address), RECORDER_HTTP_TIMEOUT_MS: "200" });
    const start = Date.now();
    await assert.rejects(ackRecord(config, validateRecord(openItem()), `0x${"ab".repeat(32)}`), (e: Error) => /timeout|abort/i.test(`${e.name} ${e.message}`));
    assert.ok(Date.now() - start < 2000, "the ack gave up at about httpTimeoutMs");
    await srv.close();
  });

  it("a stop ends the log scan between chunks (H4)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, PROGRAM, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });
    for (let i = 0; i < 6; i++) {
      const h = await wallet.sendTransaction({ account: hd0, chain, to: hd0.address, value: 0n });
      await pub.waitForTransactionReceipt({ hash: h });
    }
    const config = loadConfig({ ...baseEnv("http://127.0.0.1:1/unused", nodePort, contract), RECORDER_LOG_CHUNK: "1", RECORDER_FROM_BLOCK: "0", RECORDER_MAX_ATTEMPTS: "1" });
    const ctx = await createContext(config, collector());
    const calls = spyGetEvents(ctx);
    ctx.stopRequested = () => calls.length >= 1;

    await assert.rejects(resolveTxHash(ctx, validateRecord(openItem())), /stop requested/);
    assert.equal(calls.length, 1, "one chunk was read, then the scan stopped");
  });

  // --- G3: two previously-untested claims -----------------------------------------------------
  it("resolveTxHash stops between retries once a stop is requested — one attempt, then rejects (G3a)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const config = loadConfig({ ...baseEnv("http://127.0.0.1:1/unused", nodePort, contract), RECORDER_MAX_ATTEMPTS: "5", RECORDER_BACKOFF_BASE_MS: "40" });
    const log = collector();
    const ctx = await createContext(config, log);

    let stop = false;
    ctx.stopRequested = () => stop;
    let attempts = 0;
    const pub = ctx.clients.publicClient as unknown as { readContract: (a: unknown) => Promise<unknown> };
    pub.readContract = async () => {
      attempts += 1;
      stop = true; // a stop is requested during/after the first attempt, before any retry
      throw new Error("transient RPC failure"); // a non-revert error, so the loop would normally retry
    };

    await assert.rejects(resolveTxHash(ctx, validateRecord(openItem())), /transient RPC failure/);
    assert.equal(attempts, 1, "exactly one attempt was made; the stop check at the top of the loop halted the retry");
  });

  it("findTxHash finds the hash for a record in every consecutive block with a 2-block chunk (G3b)", { timeout: 60000 }, async () => {
    const contract = await deployFresh(nodePort);
    const srv = createFakeServer();
    const apiUrl = await srv.listen();

    const chain = localChain(nodePort);
    const wallet = createWalletClient({ account: hd0, chain, transport: viemHttp(rpcUrl(nodePort)) });
    const pub = createPublicClient({ chain, transport: viemHttp(rpcUrl(nodePort)) }) as PublicClient;

    // open, then four seals, each in its own auto-mined block (four consecutive blocks).
    const oh = await wallet.writeContract({ address: contract, abi: STRATEGY_RECORD_ABI, account: hd0, chain, functionName: "open", args: [DID, PROGRAM, 25000n] });
    await pub.waitForTransactionReceipt({ hash: oh });
    for (let r = 1; r <= 4; r++) {
      const s = sealItem(r);
      const h = await wallet.writeContract({
        address: contract,
        abi: STRATEGY_RECORD_ABI,
        account: hd0,
        chain,
        functionName: "seal",
        args: [DID, BigInt(r), s.won as boolean, BigInt(s.pnl_cents as number), JOURNAL],
      });
      await pub.waitForTransactionReceipt({ hash: h });
    }

    // All four seals are already on chain and pending; with a 2-block chunk the recorder must find
    // each one's tx hash (none may fall in a gap between chunks) and ack it without re-sending.
    srv.setPending([sealItem(1), sealItem(2), sealItem(3), sealItem(4)]);
    const config = loadConfig({ ...baseEnv(apiUrl, nodePort, contract), RECORDER_LOG_CHUNK: "2", RECORDER_FROM_BLOCK: "0" });
    const log = collector();
    const ctx = await createContext(config, log);

    await pollOnce(ctx);

    for (const r of [1, 2, 3, 4]) {
      assert.ok(srv.acks.some((a) => a.round_id === r), `seal ${r} (already on chain) was found and acked`);
    }
    assert.equal(await sealedCount(nodePort, contract), 4, "no seal was re-sent");
    await srv.close();
  });
});
