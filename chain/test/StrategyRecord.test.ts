// Every id, hash and amount in these tests is invented. Nothing here is a reading of any real
// deployment. Tests run on Hardhat's in-memory chain via `network.create()`.
import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { network } from "hardhat";
import { decodeEventLog, getAddress, keccak256, toHex } from "viem";
import type { Abi } from "viem";

const DID = keccak256(toHex("deployment:strat-1:42"));
const DID2 = keccak256(toHex("deployment:strat-2:7"));
const PROGRAM = keccak256(toHex("program-ir-v0.3"));
const JOURNAL = keccak256(toHex("journal-round-0"));

/** The custom-error name a reverted call carried, dug out of viem's nested error. */
function errorName(e: unknown): string {
  let cur = e as { cause?: unknown; data?: { errorName?: string }; message?: string } | undefined;
  for (let depth = 0; cur && depth < 12; depth++) {
    const data = cur.data;
    if (data && typeof data === "object" && typeof data.errorName === "string") return data.errorName;
    if ((cur as { name?: string }).name === "ContractFunctionRevertedError" && data?.errorName) return data.errorName;
    cur = (cur as { cause?: unknown }).cause as typeof cur;
  }
  return String((e as { message?: string })?.message ?? e);
}

async function refused(p: Promise<unknown>, name: string): Promise<void> {
  try {
    await p;
  } catch (e) {
    const got = errorName(e);
    assert.ok(got.includes(name), `expected revert ${name}, got: ${got}`);
    return;
  }
  assert.fail(`expected revert ${name}, but the call succeeded`);
}

describe("StrategyRecord", async () => {
  const { viem } = await network.create();
  const [recorder, stranger] = await viem.getWalletClients();
  const publicClient = await viem.getPublicClient();

  async function deploy() {
    return viem.deployContract("StrategyRecord", [recorder.account.address]);
  }

  it("sets the recorder address at construction (immutable)", async () => {
    const c = await deploy();
    assert.equal(getAddress(await c.read.recorder()), getAddress(recorder.account.address));
  });

  it("only the recorder can open", async () => {
    const c = await deploy();
    await refused(c.write.open([DID, PROGRAM, 1000n], { account: stranger.account }), "NotRecorder");
    assert.equal(await c.read.opened([DID]), false);
  });

  it("only the recorder can seal", async () => {
    const c = await deploy();
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    await refused(
      c.write.seal([DID, 0n, true, 100n, JOURNAL], { account: stranger.account }),
      "NotRecorder",
    );
    assert.equal(await c.read.isSealed([DID, 0n]), false);
  });

  it("a second open for the same id reverts", async () => {
    const c = await deploy();
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    await refused(c.write.open([DID, PROGRAM, 1000n], { account: recorder.account }), "AlreadyOpened");
  });

  it("a second seal for the same (id, round) reverts", async () => {
    const c = await deploy();
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    await c.write.seal([DID, 3n, true, 50n, JOURNAL], { account: recorder.account });
    await refused(c.write.seal([DID, 3n, false, -50n, JOURNAL], { account: recorder.account }), "AlreadySealed");
  });

  it("a seal before open reverts", async () => {
    const c = await deploy();
    await refused(c.write.seal([DID, 0n, true, 100n, JOURNAL], { account: recorder.account }), "NotOpened");
  });

  it("Opened carries exactly the open arguments", async () => {
    const c = await deploy();
    const hash = await c.write.open([DID, PROGRAM, 1234n], { account: recorder.account });
    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    const decoded = receipt.logs
      .map((l) => decodeEventLog({ abi: c.abi as Abi, data: l.data, topics: l.topics }))
      .find((d) => d.eventName === "Opened") as { args: Record<string, unknown> } | undefined;
    assert.ok(decoded, "no Opened event");
    assert.equal(decoded.args.deploymentId, DID);
    assert.equal(decoded.args.programHash, PROGRAM);
    assert.equal(decoded.args.budgetCents, 1234n);
  });

  it("Sealed carries exactly the seal arguments", async () => {
    const c = await deploy();
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    const hash = await c.write.seal([DID, 9n, true, 777n, JOURNAL], { account: recorder.account });
    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    const decoded = receipt.logs
      .map((l) => decodeEventLog({ abi: c.abi as Abi, data: l.data, topics: l.topics }))
      .find((d) => d.eventName === "Sealed") as { args: Record<string, unknown> } | undefined;
    assert.ok(decoded, "no Sealed event");
    assert.equal(decoded.args.deploymentId, DID);
    assert.equal(decoded.args.roundId, 9n);
    assert.equal(decoded.args.won, true);
    assert.equal(decoded.args.pnlCents, 777n);
    assert.equal(decoded.args.journalHash, JOURNAL);
  });

  it("a negative pnlCents round-trips through the Sealed event", async () => {
    const c = await deploy();
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    const hash = await c.write.seal([DID, 1n, false, -4242n, JOURNAL], { account: recorder.account });
    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    const decoded = receipt.logs
      .map((l) => decodeEventLog({ abi: c.abi as Abi, data: l.data, topics: l.topics }))
      .find((d) => d.eventName === "Sealed") as { args: Record<string, unknown> } | undefined;
    assert.ok(decoded, "no Sealed event");
    assert.equal(decoded.args.won, false);
    assert.equal(decoded.args.pnlCents, -4242n);
  });

  it("opened and sealed read back true after recording", async () => {
    const c = await deploy();
    assert.equal(await c.read.opened([DID]), false);
    await c.write.open([DID, PROGRAM, 1000n], { account: recorder.account });
    assert.equal(await c.read.opened([DID]), true);

    assert.equal(await c.read.isSealed([DID, 5n]), false);
    await c.write.seal([DID, 5n, true, 10n, JOURNAL], { account: recorder.account });
    assert.equal(await c.read.isSealed([DID, 5n]), true);

    // An untouched id / round stays false.
    assert.equal(await c.read.opened([DID2]), false);
    assert.equal(await c.read.isSealed([DID, 6n]), false);
  });
});
