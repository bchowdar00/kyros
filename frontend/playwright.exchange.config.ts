import { defineConfig, devices } from '@playwright/test'

// Opt-in proof against an already running local development stack, never a funded venue.
export default defineConfig({
  testDir: './e2e',
  testMatch: '**/exchange-local.dev.ts',
  workers: 1,
  retries: 0,
  timeout: 60000,
  use: {
    ...devices['Desktop Chrome'],
    baseURL: process.env['EXCHANGE_TEST_BASE_URL'] ?? 'http://localhost:3000',
    viewport: { width: 1440, height: 1000 },
  },
})
