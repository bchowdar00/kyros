import { describe, expect, it } from 'vitest'

import { buildContentSecurityPolicy, createNonce } from './csp'

describe('buildContentSecurityPolicy', () => {
  const nonce = 'dGVzdC1ub25jZQ=='
  const production = { isDevelopment: false, isSecureOrigin: true, nonce }

  it('locks scripts to the request nonce in production', () => {
    const policy = buildContentSecurityPolicy(production)

    expect(policy).toContain(`script-src 'self' 'nonce-${nonce}' 'strict-dynamic'`)
    expect(policy).not.toContain('unsafe-eval')
    expect(policy).toContain('upgrade-insecure-requests')
    expect(policy).toContain("frame-ancestors 'self'")
    expect(policy).toContain("object-src 'none'")
  })

  it('allows eval and websockets only in development', () => {
    const policy = buildContentSecurityPolicy({ ...production, isDevelopment: true })

    expect(policy).toContain("'unsafe-eval'")
    expect(policy).toContain("connect-src 'self' ws: wss:")
  })

  it('only upgrades insecure requests when the page itself is secure', () => {
    const policy = buildContentSecurityPolicy({ ...production, isSecureOrigin: false })

    expect(policy).not.toContain('upgrade-insecure-requests')
  })

  it('adds the API origin to connect-src', () => {
    const policy = buildContentSecurityPolicy({
      ...production,
      connectSources: ['https://api.kyros.example'],
    })

    expect(policy).toContain("connect-src 'self' https://api.kyros.example")
  })
})

describe('createNonce', () => {
  it('is unique per call and safe inside a header', () => {
    const first = createNonce()
    const second = createNonce()

    expect(first).not.toBe(second)
    expect(first).toMatch(/^[A-Za-z0-9+/=]+$/)
  })
})
