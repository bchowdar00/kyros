import { defineConfig, devices } from '@playwright/test'

const port = 3100
const baseURL = `http://127.0.0.1:${port}`
const isCI = process.env['CI'] === 'true'

// Runs against the production build (`next start`), so budgets, headers and hydration are
// measured on what ships. Desktop Chrome only while the product is desktop-first
// (AGENTS.md §0.2); mobile.e2e.ts covers the small-viewport coming-soon screen explicitly.
export default defineConfig({
  forbidOnly: isCI,
  fullyParallel: true,
  projects: [{ name: 'desktop-chrome', use: { ...devices['Desktop Chrome'] } }],
  reporter: isCI ? [['github'], ['html', { open: 'never' }]] : [['list']],
  retries: isCI ? 1 : 0,
  testDir: './e2e',
  testMatch: /.*\.e2e\.ts$/,
  use: { baseURL, trace: 'retain-on-failure' },
  webServer: {
    command: `pnpm exec next start --port ${port}`,
    reuseExistingServer: !isCI,
    timeout: 60_000,
    url: baseURL,
  },
})
