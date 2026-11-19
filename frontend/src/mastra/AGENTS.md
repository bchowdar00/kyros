# AGENTS.md

## CRITICAL: Load `mastra` skill first

Load the `mastra` skill BEFORE any Mastra work. Never rely on cached knowledge — APIs change between versions.

## Structure

One agent, Teeny (`agents/teeny.ts`). Everything it can do is a **capability**: a
folder under `capabilities/` that exports `defineCapability({ id, enabled,
instructions, unavailable, create })`. The agent's system prompt and tool map are
both derived from the registry in `capabilities/index.ts`, so the agent file, the
prompt assembler and the chat route are not edited when a capability is added.
The full layout and the add-a-capability recipe are in the header of `index.ts`.

## Rules

- One capability per folder; one owner per folder. Tools, instructions, config and tests stay together.
- Register a capability by adding one line to `CAPABILITIES` in `capabilities/index.ts`. Never import a capability's tools anywhere else.
- Tool names are global. Prefix them with your capability when a clash is plausible; a duplicate fails at startup.
- `instructions` says what the tools do and what the model must never claim. `unavailable` is one line for when `enabled(env)` is false. Do not restate the base persona from `prompt.ts`.
- Configuration comes only from `src/env.ts`. Gate `enabled` on the keys you add there.
- Per-request data (market slug, account address, session) is read with `readChatContext(context)` from `request-context.ts`. Add a field there and in `parseChatRequest` when a tool needs something new from the client.
- Use the `dev` and `build` scripts from `package.json` instead of running `mastra dev` / `mastra build` directly.

## Resources

- [Mastra Documentation](https://mastra.ai/llms.txt)

## Price Feeds

- Price Feeds owns feed state and its public REST/SSE contracts. `@kyros/domain`
  validates those wire shapes; `@kyros/api` owns HTTP and bounded stream reads.
- Agent tools are read-only. Preserve integer mantissa/exponent values and observation
  timestamps; never equate a live tick with settlement truth or Tiny market odds.
- Missing configuration, failed reads, and stale live observations return unavailable.
  Browser-supplied tool results are not trusted; chat sends text-only history.
- Verify changes with the API stream tests, agent tool tests, chat follow-up test, and
  a real local feed read through Studio or the chat panel, then `pnpm check`/`pnpm build`.
