// Every id, address and amount in these tests is invented pretend-money. Nothing here is a reading
// of any real wallet. Tests run on Hardhat's in-memory chain via `network.create()`.
import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { network } from "hardhat";
import { decodeEventLog, getAddress, keccak256, toHex } from "viem";
import type { Abi } from "viem";

const SID = keccak256(toHex("deployment:strat-1:42"));
const SID2 = keccak256(toHex("deployment:strat-2:7"));
const SID3 = keccak256(toHex("deployment:strat-3:99"));

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
    // Exact, not a substring: "WalletExists" is inside "SubWalletExists".
    assert.equal(got, name, `expected revert ${name}, got: ${got}`);
    return;
  }
  assert.fail(`expected revert ${name}, but the call succeeded`);
}

describe("StrategyWallets", async () => {
  const { viem } = await network.create();
  const [recorder, stranger, ownerA, ownerB] = await viem.getWalletClients();
  const publicClient = await viem.getPublicClient();

  async function deploy() {
    return viem.deployContract("StrategyWallets", [recorder.account.address]);
  }

  /** Decode the one event of `name` emitted by the tx at `hash`. */
  async function eventOf(c: { abi: unknown }, hash: `0x${string}`, name: string) {
    const receipt = await publicClient.waitForTransactionReceipt({ hash });
    const decoded = receipt.logs
      .map((l) => decodeEventLog({ abi: c.abi as Abi, data: l.data, topics: l.topics }))
      .find((d) => d.eventName === name) as { args: Record<string, unknown> } | undefined;
    assert.ok(decoded, `no ${name} event`);
    return decoded.args;
  }

  it("sets the recorder address at construction (immutable)", async () => {
    const c = await deploy();
    assert.equal(getAddress(await c.read.recorder()), getAddress(recorder.account.address));
  });

  // 1. only the recorder can call each of the four writers.
  it("only the recorder can call each writer", async () => {
    const c = await deploy();
    await refused(
      c.write.openWallet([ownerA.account.address, 100000n], { account: stranger.account }),
      "NotRecorder",
    );
    // Open a wallet + sub-wallet as recorder so the other three writers reach their guards.
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await refused(
      c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: stranger.account }),
      "NotRecorder",
    );
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    await refused(c.write.settle([SID, 0n, 10n], { account: stranger.account }), "NotRecorder");
    await refused(c.write.closeSubWallet([SID], { account: stranger.account }), "NotRecorder");
  });

  // 2. a wallet opens once; a second open reverts WalletExists.
  it("a wallet opens once; a second open reverts", async () => {
    const c = await deploy();
    const hash = await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    const args = await eventOf(c, hash, "WalletOpened");
    assert.equal(getAddress(args.owner as string), getAddress(ownerA.account.address));
    assert.equal(args.balanceCents, 100000n);

    const [exists, balance] = await c.read.walletOf([ownerA.account.address]);
    assert.equal(exists, true);
    assert.equal(balance, 100000n);

    await refused(
      c.write.openWallet([ownerA.account.address, 1n], { account: recorder.account }),
      "WalletExists",
    );
  });

  // 3. opening a sub-wallet moves the budget out of the wallet and emits SubWalletOpened.
  it("opening a sub-wallet moves the budget out of the wallet", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    const hash = await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    const args = await eventOf(c, hash, "SubWalletOpened");
    assert.equal(args.deploymentId, SID);
    assert.equal(getAddress(args.owner as string), getAddress(ownerA.account.address));
    assert.equal(args.budgetCents, 4000n);

    const [, walletBalance] = await c.read.walletOf([ownerA.account.address]);
    assert.equal(walletBalance, 96000n);

    const [subExists, owner, budget, subBalance, closed] = await c.read.subWalletOf([SID]);
    assert.equal(subExists, true);
    assert.equal(getAddress(owner as string), getAddress(ownerA.account.address));
    assert.equal(budget, 4000n);
    assert.equal(subBalance, 4000n);
    assert.equal(closed, false);
  });

  // 4. a sub-wallet cannot be opened without a wallet, twice with the same id, or over budget.
  it("openSubWallet guards: no wallet, duplicate id, over budget", async () => {
    const c = await deploy();
    // No wallet yet.
    await refused(
      c.write.openSubWallet([SID, ownerA.account.address, 10n], { account: recorder.account }),
      "NoWallet",
    );
    await c.write.openWallet([ownerA.account.address, 5000n], { account: recorder.account });
    // Over budget: wallet holds 5000, ask for 5001.
    await refused(
      c.write.openSubWallet([SID, ownerA.account.address, 5001n], { account: recorder.account }),
      "InsufficientBalance",
    );
    // First open succeeds; a second with the same id reverts.
    await c.write.openSubWallet([SID, ownerA.account.address, 1000n], { account: recorder.account });
    await refused(
      c.write.openSubWallet([SID, ownerA.account.address, 1000n], { account: recorder.account }),
      "SubWalletExists",
    );
    // A zero budget is allowed.
    await c.write.openSubWallet([SID2, ownerA.account.address, 0n], { account: recorder.account });
    const [, , , zeroBal] = await c.read.subWalletOf([SID2]);
    assert.equal(zeroBal, 0n);
  });

  // 5. settling applies gains and losses to the sub-wallet only and emits the new balance.
  it("settle applies pnl to the sub-wallet only", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });

    const h1 = await c.write.settle([SID, 0n, 120n], { account: recorder.account });
    const a1 = await eventOf(c, h1, "SubWalletSettled");
    assert.equal(a1.deploymentId, SID);
    assert.equal(a1.roundId, 0n);
    assert.equal(a1.pnlCents, 120n);
    assert.equal(a1.balanceCents, 4120n);

    const h2 = await c.write.settle([SID, 1n, -996n], { account: recorder.account });
    const a2 = await eventOf(c, h2, "SubWalletSettled");
    assert.equal(a2.pnlCents, -996n);
    assert.equal(a2.balanceCents, 3124n);

    // Sub-wallet balance moved; the owner's wallet is untouched by settlement.
    const [, , , subBalance] = await c.read.subWalletOf([SID]);
    assert.equal(subBalance, 3124n);
    const [, walletBalance] = await c.read.walletOf([ownerA.account.address]);
    assert.equal(walletBalance, 96000n);

    assert.equal(await c.read.settled([SID, 0n]), true);
    assert.equal(await c.read.settled([SID, 1n]), true);
  });

  // 6. a round settles once.
  it("a round settles once", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    await c.write.settle([SID, 3n, 50n], { account: recorder.account });
    await refused(c.write.settle([SID, 3n, -50n], { account: recorder.account }), "RoundAlreadySettled");
  });

  // 7. a sub-wallet cannot lose more than it holds, and the failed settle changes nothing.
  it("Overdrawn settle is rejected and changes nothing", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    // Balance 4000; a loss of 4001 would go negative.
    await refused(c.write.settle([SID, 7n, -4001n], { account: recorder.account }), "Overdrawn");

    const [, , , subBalance] = await c.read.subWalletOf([SID]);
    assert.equal(subBalance, 4000n);
    assert.equal(await c.read.settled([SID, 7n]), false);

    // Exactly to zero is allowed (not an overdraw).
    const hash = await c.write.settle([SID, 7n, -4000n], { account: recorder.account });
    const args = await eventOf(c, hash, "SubWalletSettled");
    assert.equal(args.balanceCents, 0n);
  });

  // 8. closing returns the remainder; closing twice, settling after close, and reopening all fail.
  it("closing returns the remainder to the wallet and is final", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    await c.write.settle([SID, 0n, 120n], { account: recorder.account });
    await c.write.settle([SID, 1n, -996n], { account: recorder.account });
    // Sub holds 3124; wallet holds 96000.

    const hash = await c.write.closeSubWallet([SID], { account: recorder.account });
    const args = await eventOf(c, hash, "SubWalletReturned");
    assert.equal(args.deploymentId, SID);
    assert.equal(getAddress(args.owner as string), getAddress(ownerA.account.address));
    assert.equal(args.returnedCents, 3124n);

    const [, walletBalance] = await c.read.walletOf([ownerA.account.address]);
    assert.equal(walletBalance, 99124n);

    const [, , , subBalance, closed] = await c.read.subWalletOf([SID]);
    assert.equal(subBalance, 0n);
    assert.equal(closed, true);

    // Closing twice reverts; settling after close reverts; the id cannot be reopened.
    await refused(c.write.closeSubWallet([SID], { account: recorder.account }), "SubWalletClosed");
    await refused(c.write.settle([SID, 2n, 1n], { account: recorder.account }), "SubWalletClosed");
    await refused(
      c.write.openSubWallet([SID, ownerA.account.address, 10n], { account: recorder.account }),
      "SubWalletExists",
    );
  });

  // 9. conservation across two sub-wallets under one owner.
  it("conservation: wallet + open sub balances == start + settled pnl", async () => {
    const c = await deploy();
    const start = 100000n;
    await c.write.openWallet([ownerA.account.address, start], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 4000n], { account: recorder.account });
    await c.write.openSubWallet([SID2, ownerA.account.address, 10000n], { account: recorder.account });

    await c.write.settle([SID, 0n, 120n], { account: recorder.account });
    await c.write.settle([SID, 1n, -996n], { account: recorder.account });
    await c.write.settle([SID2, 0n, 500n], { account: recorder.account });
    const totalPnl = 120n - 996n + 500n;

    // Close SID (balance 3124), leaving SID2 open.
    await c.write.closeSubWallet([SID], { account: recorder.account });

    const [, walletBalance] = await c.read.walletOf([ownerA.account.address]);
    const [, , , sid2Balance] = await c.read.subWalletOf([SID2]);
    assert.equal(walletBalance + sid2Balance, start + totalPnl);
  });

  // 10. subWalletsOf lists an owner's ids in order opened; another owner's list is separate.
  it("subWalletsOf lists ids in order, per owner", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100000n], { account: recorder.account });
    await c.write.openWallet([ownerB.account.address, 100000n], { account: recorder.account });

    await c.write.openSubWallet([SID, ownerA.account.address, 1000n], { account: recorder.account });
    await c.write.openSubWallet([SID2, ownerA.account.address, 1000n], { account: recorder.account });
    await c.write.openSubWallet([SID3, ownerB.account.address, 1000n], { account: recorder.account });

    const listA = await c.read.subWalletsOf([ownerA.account.address]);
    const listB = await c.read.subWalletsOf([ownerB.account.address]);
    assert.deepEqual(listA, [SID, SID2]);
    assert.deepEqual(listB, [SID3]);
  });

  const MAX = 2n ** 64n - 1n;
  const ZERO = "0x0000000000000000000000000000000000000000";

  it("a settle that would pass the largest balance is refused, not wrapped round to zero", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, MAX], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, MAX], { account: recorder.account });

    await refused(c.write.settle([SID, 0n, 1n], { account: recorder.account }), "BalanceTooLarge");

    const [, , , subBalance] = await c.read.subWalletOf([SID]);
    assert.equal(subBalance, MAX);
    assert.equal(await c.read.settled([SID, 0n]), false);
  });

  it("a sub-wallet can always be closed, however much its wallet already holds", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, MAX], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 1n], { account: recorder.account });
    await c.write.settle([SID, 0n, 10n], { account: recorder.account });
    // Wallet holds MAX - 1 and the sub-wallet 11: together more than a uint64 can carry.

    await c.write.closeSubWallet([SID], { account: recorder.account });

    const [, walletBalance] = await c.read.walletOf([ownerA.account.address]);
    assert.equal(walletBalance, MAX + 10n);
  });

  it("an id that was never opened is told apart from an open, empty sub-wallet", async () => {
    const c = await deploy();
    await c.write.openWallet([ownerA.account.address, 100n], { account: recorder.account });
    await c.write.openSubWallet([SID, ownerA.account.address, 0n], { account: recorder.account });

    const [opened] = await c.read.subWalletOf([SID]);
    const [never] = await c.read.subWalletOf([SID3]);
    assert.equal(opened, true);
    assert.equal(never, false);

    const [walletExists, walletBalance] = await c.read.walletOf([ownerB.account.address]);
    assert.equal(walletExists, false);
    assert.equal(walletBalance, 0n);
  });

  it("settling or closing an id that was never opened is refused", async () => {
    const c = await deploy();
    await refused(c.write.settle([SID3, 0n, 1n], { account: recorder.account }), "NoSubWallet");
    await refused(c.write.closeSubWallet([SID3], { account: recorder.account }), "NoSubWallet");
  });

  it("the zero address can be neither the recorder nor a wallet's owner", async () => {
    // A constructor's revert reaches the client undecoded, so only the refusal is checked here.
    await assert.rejects(viem.deployContract("StrategyWallets", [ZERO]));
    const c = await deploy();
    await refused(c.write.openWallet([ZERO, 100n], { account: recorder.account }), "ZeroAddress");
  });
});
