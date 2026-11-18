// The deployment entry the owner runs by hand, holding the signing key in Hardhat's keystore:
//
//   cd chain && npx hardhat run scripts/deploy.ts --network arcTestnet
//
// It refuses to do anything unless it is talking to Arc Testnet (chain 5042002). It prints the
// deployer address and the recorder address it will set (from env RECORDER_ADDRESS — an address,
// never a key) before sending, deploys StrategyRecord with that recorder immutable, reads the
// recorder back, and prints the deployed address. No key is read, printed, or stored here.
import { network } from "hardhat";
import { formatUnits, getAddress, isAddress } from "viem";
import type { Address } from "viem";

const ARC_CHAIN_ID = 5042002;

async function main(): Promise<void> {
  const { viem } = await network.connect();
  const publicClient = await viem.getPublicClient();
  const chainId = await publicClient.getChainId();
  if (chainId !== ARC_CHAIN_ID) {
    console.error(`Refusing to deploy: connected to chain ${chainId}, but this script only deploys to Arc Testnet (${ARC_CHAIN_ID}).`);
    console.error("Run it with: cd chain && npx hardhat run scripts/deploy.ts --network arcTestnet");
    process.exit(1);
  }

  const recorderRaw = process.env.RECORDER_ADDRESS;
  if (!recorderRaw || !isAddress(recorderRaw)) {
    console.error("Set RECORDER_ADDRESS to the recorder's address (an address, not a key) before deploying.");
    process.exit(1);
  }
  const recorder = getAddress(recorderRaw) as Address;

  const deployer = (await viem.getWalletClients())[0].account.address;
  const balance = await publicClient.getBalance({ address: deployer });
  console.log(`deployer       ${deployer}`);
  console.log(`balance        ${formatUnits(balance, 18)} (USDC, Arc's gas token)`);
  console.log(`recorder       ${recorder}`);
  console.log("deploying StrategyRecord...");

  const contract = await viem.deployContract("StrategyRecord", [recorder]);
  const readBack = await contract.read.recorder();
  if (getAddress(readBack) !== recorder) {
    console.error(`Deployed, but the recorder reads back as ${readBack}, not ${recorder}.`);
    process.exit(1);
  }
  console.log(`StrategyRecord ${contract.address}`);
  console.log("done.");
}

await main();
