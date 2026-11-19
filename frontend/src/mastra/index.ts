/**
 * Kyros, Tiny's chat agent, and how this folder is organized.
 *
 *   mastra/
 *     index.ts              this file: the Mastra instance (storage, observability)
 *     agents/teeny.ts       the chat agent; derives its prompt and tools from the registry
 *     agents/strategy-writer.ts  the tool-less writer the compile workflow calls
 *     workflows/compile-strategy.ts  brief -> signals -> document -> compiled program
 *     prompt.ts             base persona + per-capability fragments -> system prompt
 *     request-context.ts    what the chat route passes to tools (market, account, session)
 *     chat-session.ts       browser identity for memory (dev-only cookie)
 *     chat-history.ts       thread create/list/read over Mastra memory
 *     capabilities/
 *       types.ts            the Capability contract and defineCapability()
 *       index.ts            THE REGISTRY: one entry per capability, in prompt order
 *       price-feeds/        one folder per capability, owned by one engineer:
 *         index.ts            defineCapability({ id, enabled, instructions, unavailable, create })
 *         tools.ts            createTool() definitions
 *         instructions.ts     the prompt fragment for this capability
 *
 * To add a capability (data query, web search, trading, strategy compilation, ...):
 *   1. Copy `capabilities/price-feeds/` to `capabilities/<your-id>/`.
 *   2. Write your tools with createTool(); read request data via readChatContext().
 *   3. Write `instructions` (what the tools do and must not claim) and `unavailable`.
 *   4. Add any env keys to `src/env.ts` and gate `enabled` on them.
 *   5. Add one line to CAPABILITIES in `capabilities/index.ts`.
 *   6. Add a test next to `src/test/price-feed-tools.test.ts`.
 * Nothing else changes: not the agent, not the prompt, not the chat route. Tool
 * names must be unique across capabilities; a duplicate fails at startup.
 */
import { mkdirSync } from 'node:fs'
import { resolve } from 'node:path'
import { Mastra } from '@mastra/core/mastra'
import { LibSQLStore } from '@mastra/libsql'
import { MastraStorageExporter, Observability, SensitiveDataFilter } from '@mastra/observability'
import { env } from '../env'
import { strategyWriter } from './agents/strategy-writer'
import { teeny, teenyTools } from './agents/teeny'
import { compileStrategyWorkflow } from './workflows/compile-strategy'

// Local runtime data is created on the host, never traced into the deployment bundle.
const storageDirectory = resolve(
  /* turbopackIgnore: true */ env.MASTRA_STORAGE_DIR || '.mastra-data'
)
mkdirSync(storageDirectory, { recursive: true })

// SQLite supports concurrent local Studio and Next.js connections. DuckDB's
// single-writer process lock prevents that when both runtimes share one file.
export const mastra = new Mastra({
  agents: { teeny, strategyWriter },
  workflows: { compileStrategyWorkflow },
  tools: teenyTools,
  storage: new LibSQLStore({
    id: 'mastra-storage',
    url: env.TURSO_DATABASE_URL || `file:${resolve(storageDirectory, 'mastra.db')}`,
    ...(env.TURSO_AUTH_TOKEN ? { authToken: env.TURSO_AUTH_TOKEN } : {}),
  }),
  observability: new Observability({
    configs: {
      default: {
        serviceName: 'teeny',
        exporters: [new MastraStorageExporter()],
        spanOutputProcessors: [new SensitiveDataFilter()],
      },
    },
  }),
})
