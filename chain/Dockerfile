# Kyros chain recorder, for Azure Container Apps.
#
# The recorder (recorder/src/index.ts) polls the Kyros server for pending
# records, writes them to the StrategyRecord contract and acks them back. It
# is a single TypeScript entry point run directly by Node 24, which strips
# the (fully erasable) types at load time — no build step and no transpiler.
# Its only runtime dependency is viem; hardhat, the contracts, the compiled
# artifacts and the tests are build-/test-time only and are not shipped.
#
# Config comes from the environment only (KYROS_API_URL, ARC_RPC_URL,
# STRATEGY_RECORD_ADDRESS, RECORDER_PRIVATE_KEY, RECORDER_TOKEN); with none
# set it exits non-zero naming the first missing one. No secrets are baked in.
#
# Build context is `chain/`. Node 24 matches @types/node 24.x in package.json.
# Architecture-neutral: no platform is named.
FROM node:24-bookworm-slim

WORKDIR /app

# Install only the runtime dependency (viem) from the committed lockfile.
# --omit=dev drops hardhat, typescript and the toolbox, none of which the
# recorder loads at run time.
COPY package.json package-lock.json ./
RUN npm ci --omit=dev

# The recorder source only; the contract, scripts, tests and hardhat config
# are left out.
COPY recorder/src ./recorder/src

# node:*-slim ships a non-root `node` user.
USER node

CMD ["node", "recorder/src/index.ts"]
