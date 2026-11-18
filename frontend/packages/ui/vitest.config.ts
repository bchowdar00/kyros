import { defineConfig } from 'vitest/config'

export default defineConfig({
  // jsdom is a browser, so dependencies that branch on the `browser` export condition
  // (number-flow's custom element) must resolve their browser build here too.
  resolve: {
    conditions: ['browser'],
  },
  ssr: {
    resolve: {
      conditions: ['browser'],
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
  },
})
