import { defineConfig } from 'vitest/config'

export default defineConfig({
  oxc: {
    jsx: {
      runtime: 'automatic',
    },
  },
  resolve: {
    alias: {
      '@': new URL('./src', import.meta.url).pathname,
      'server-only': new URL('./src/test/server-only.ts', import.meta.url).pathname,
    },
  },
  test: {
    projects: [
      {
        extends: true,
        resolve: { conditions: ['browser'] },
        ssr: { resolve: { conditions: ['browser'] } },
        test: {
          name: 'browser',
          environment: 'jsdom',
          include: ['src/**/*.test.{ts,tsx}'],
          exclude: ['src/**/*.server.test.ts'],
          setupFiles: ['./src/test/setup.ts'],
        },
      },
      {
        extends: true,
        resolve: { conditions: ['node'] },
        ssr: { resolve: { conditions: ['node'] } },
        test: {
          name: 'server',
          environment: 'node',
          include: ['src/**/*.server.test.ts'],
        },
      },
    ],
  },
})
