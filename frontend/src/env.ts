import { createEnv } from '@t3-oss/env-nextjs'
import * as z from 'zod/mini'
import { WRITER_MODELS } from './mastra/writer-models'

declare global {
  namespace NodeJS {
    interface ProcessEnv {
      readonly NEXT_PUBLIC_API_URL?: string
      readonly NEXT_PUBLIC_ERROR_REPORT_URL?: string
      readonly NEXT_PUBLIC_EXPLORER_URL?: string
      readonly NEXT_PUBLIC_PRICE_FEEDS_URL?: string
      readonly NEXT_PUBLIC_SITE_URL?: string
      readonly NEXT_PUBLIC_VITALS_URL?: string
      readonly EXCHANGE_API_URL?: string
      readonly SILKROAD_API_URL?: string
      readonly SKIP_ENV_VALIDATION?: string
      readonly CHAT_ENABLED?: string
      readonly HIDDEN_UNDERLYINGS?: string
      readonly ANTHROPIC_API_KEY?: string
      readonly WRITER_MODEL?: string
      readonly WRITER_METER_TOKEN?: string
    }
  }
}

/**
 * The only module allowed to read `process.env` (enforced by oxlint `node/no-process-env`).
 * Public variables are inlined at build time, which is why each one is spelled out below
 * instead of being looked up dynamically.
 */
export const env = createEnv({
  shared: {
    NODE_ENV: z._default(z.enum(['development', 'production', 'test']), 'development'),
  },
  server: {
    EXCHANGE_DEMO_UI_URL: z.optional(z.url()),
    EXCHANGE_DEMO_GATEWAY_URL: z.optional(z.url()),
    EXCHANGE_DEMO_FEED_URL: z.optional(z.url()),
    /**
     * Server-only Claude key (from the host environment) for Kyros chat and the strategy
     * writer. Both agents run on `WRITER_MODEL`. Unset disables the chat and build routes.
     */
    ANTHROPIC_API_KEY: z.optional(z.string()),
    /**
     * Which Claude model the writer runs. One of three; any other value fails validation
     * at boot. Defaults to Opus 5.5.
     */
    WRITER_MODEL: z._default(z.enum(WRITER_MODELS), WRITER_MODELS[0]),
    /**
     * Shared secret sent to the exchange's writer meter as `x-writer-token`. Must match the
     * exchange's `WRITER_METER_TOKEN`; unset on either side, the meter is closed and the writer is off.
     */
    WRITER_METER_TOKEN: z.optional(z.string()),
    /**
     * `1` serves the Kyros chat routes outside development. Chat identity is an anonymous
     * browser cookie, not account authorization, so this exposes the Claude key's spend to
     * every visitor; enable it only on a demo deployment.
     */
    CHAT_ENABLED: z.optional(z.string()),
    /**
     * Comma-separated underlyings (`btc`) whose series the site hides: they are
     * dropped from the catalog loader, so the home grid, market pages and the active-market
     * route behave as if the exchange did not run them. The backend is unchanged. Used by
     * the BTC-only demo deployment.
     */
    HIDDEN_UNDERLYINGS: z.optional(z.string()),
    PRICE_FEEDS_API_URL: z.optional(z.url()),
    MASTRA_STORAGE_DIR: z.optional(z.string()),
    TURSO_DATABASE_URL: z.optional(z.string()),
    TURSO_AUTH_TOKEN: z.optional(z.string()),
    /**
     * Rust exchange origin (`server/`) for server loaders: series catalog, round history,
     * stats. Unset in development means the static fixtures render instead.
     */
    EXCHANGE_API_URL: z.optional(z.url()),
    /**
     * The `StrategyRecord` contract on Arc Testnet whose transactions the Records page is
     * built from. Defaults to the deployed one (README).
     */
    RECORD_CONTRACT_ADDRESS: z._default(
      z.string().check(z.regex(/^0x[0-9a-fA-F]{40}$/)),
      '0xc910cfe6b537feee16b60a9964c55fc66580586a'
    ),
    /** Superseded by EXCHANGE_API_URL; kept so existing deployments do not fail validation. */
    SILKROAD_API_URL: z.optional(z.url()),
  },
  client: {
    /**
     * Rust exchange origin for the browser: REST orders and the `/ws/*` streams. Its http and
     * ws forms are both added to the CSP `connect-src`.
     */
    NEXT_PUBLIC_API_URL: z.optional(z.url()),
    /** Price Feeds origin for live SSE. Also added to the CSP `connect-src`. */
    NEXT_PUBLIC_PRICE_FEEDS_URL: z.optional(z.url()),
    /** Where client-side errors are beaconed. Unset means errors stay in the console. */
    NEXT_PUBLIC_ERROR_REPORT_URL: z.optional(z.url()),
    /** Block explorer for sealed strategy records (Arc Testnet). */
    NEXT_PUBLIC_EXPLORER_URL: z._default(z.url(), 'https://explorer.testnet.arc.io'),
    /** Canonical origin for metadata. */
    NEXT_PUBLIC_SITE_URL: z._default(z.url(), 'http://localhost:3000'),
    /** Where Web Vitals are beaconed. Unset means they are not reported. */
    NEXT_PUBLIC_VITALS_URL: z.optional(z.url()),
  },
  experimental__runtimeEnv: {
    NEXT_PUBLIC_API_URL: process.env.NEXT_PUBLIC_API_URL,
    NEXT_PUBLIC_ERROR_REPORT_URL: process.env.NEXT_PUBLIC_ERROR_REPORT_URL,
    NEXT_PUBLIC_EXPLORER_URL: process.env.NEXT_PUBLIC_EXPLORER_URL,
    NEXT_PUBLIC_PRICE_FEEDS_URL: process.env.NEXT_PUBLIC_PRICE_FEEDS_URL,
    NEXT_PUBLIC_SITE_URL: process.env.NEXT_PUBLIC_SITE_URL,
    NEXT_PUBLIC_VITALS_URL: process.env.NEXT_PUBLIC_VITALS_URL,
    NODE_ENV: process.env.NODE_ENV,
  },
  emptyStringAsUndefined: true,
  skipValidation: process.env.SKIP_ENV_VALIDATION === '1',
})
