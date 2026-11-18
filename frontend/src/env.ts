import { createEnv } from '@t3-oss/env-nextjs'
import * as z from 'zod/mini'

declare global {
  namespace NodeJS {
    interface ProcessEnv {
      readonly NEXT_PUBLIC_API_URL?: string
      readonly NEXT_PUBLIC_ERROR_REPORT_URL?: string
      readonly NEXT_PUBLIC_PRICE_FEEDS_URL?: string
      readonly NEXT_PUBLIC_SITE_URL?: string
      readonly NEXT_PUBLIC_VITALS_URL?: string
      readonly EXCHANGE_API_URL?: string
      readonly SILKROAD_API_URL?: string
      readonly SKIP_ENV_VALIDATION?: string
      readonly CHAT_ENABLED?: string
      readonly HIDDEN_UNDERLYINGS?: string
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
    OPENAI_API_KEY: z.optional(z.string()),
    /**
     * `1` serves the Teeny chat routes outside development. Chat identity is an anonymous
     * browser cookie, not account authorization, so this exposes the OpenAI key's spend to
     * every visitor; enable it only on a demo deployment.
     */
    CHAT_ENABLED: z.optional(z.string()),
    /**
     * Comma-separated underlyings (`nba`, `btc`) whose series the site hides: they are
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
    /** Canonical origin for metadata. */
    NEXT_PUBLIC_SITE_URL: z._default(z.url(), 'http://localhost:3000'),
    /** Where Web Vitals are beaconed. Unset means they are not reported. */
    NEXT_PUBLIC_VITALS_URL: z.optional(z.url()),
  },
  experimental__runtimeEnv: {
    NEXT_PUBLIC_API_URL: process.env.NEXT_PUBLIC_API_URL,
    NEXT_PUBLIC_ERROR_REPORT_URL: process.env.NEXT_PUBLIC_ERROR_REPORT_URL,
    NEXT_PUBLIC_PRICE_FEEDS_URL: process.env.NEXT_PUBLIC_PRICE_FEEDS_URL,
    NEXT_PUBLIC_SITE_URL: process.env.NEXT_PUBLIC_SITE_URL,
    NEXT_PUBLIC_VITALS_URL: process.env.NEXT_PUBLIC_VITALS_URL,
    NODE_ENV: process.env.NODE_ENV,
  },
  emptyStringAsUndefined: true,
  skipValidation: process.env.SKIP_ENV_VALIDATION === '1',
})
