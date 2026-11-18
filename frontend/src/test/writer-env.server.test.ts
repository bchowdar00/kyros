import { afterEach, describe, expect, it, vi } from 'vitest'
import { WRITER_MODELS } from '@/mastra/writer-models'

afterEach(() => {
  vi.unstubAllEnvs()
  vi.resetModules()
})

async function loadEnv() {
  vi.resetModules()
  const loaded = await import('@/env')
  return loaded.env
}

describe('WRITER_MODEL validation in env.ts', () => {
  it('rejects a model outside the three', async () => {
    vi.stubEnv('WRITER_MODEL', 'gpt-5.6-terra')
    await expect(loadEnv()).rejects.toThrow()
  })

  it('defaults to claude-opus-5-5 when unset', async () => {
    vi.stubEnv('WRITER_MODEL', '')
    const env = await loadEnv()
    expect(env.WRITER_MODEL).toBe('claude-opus-5-5')
  })

  it('accepts each of the three models', async () => {
    for (const model of WRITER_MODELS) {
      vi.stubEnv('WRITER_MODEL', model)
      const env = await loadEnv()
      expect(env.WRITER_MODEL).toBe(model)
    }
  })
})
