import { expect, test } from '@playwright/test'

test.describe('security headers', () => {
  test('every document carries the strict headers', async ({ page }) => {
    const response = await page.goto('/')
    const headers = response?.headers() ?? {}

    expect(headers['x-powered-by']).toBeUndefined()
    expect(headers['strict-transport-security']).toContain('max-age=63072000')
    expect(headers['x-content-type-options']).toBe('nosniff')
    expect(headers['referrer-policy']).toBe('strict-origin-when-cross-origin')
    expect(headers['permissions-policy']).toContain('camera=()')

    const policy = headers['content-security-policy'] ?? ''
    expect(policy).toMatch(/script-src 'self' 'nonce-[A-Za-z0-9+/=]+' 'strict-dynamic'/)
    expect(policy).not.toContain('unsafe-eval')
    expect(policy).toContain("frame-ancestors 'self'")
    expect(policy).toContain("object-src 'none'")
  })

  test('the nonce changes on every request', async ({ request }) => {
    const first = (await request.get('/')).headers()['content-security-policy']
    const second = (await request.get('/')).headers()['content-security-policy']

    expect(first).toBeDefined()
    expect(first).not.toBe(second)
  })

  test('every script tag is nonced, so nothing runs outside the policy', async ({ page }) => {
    await page.goto('/')

    const unnonced = await page.evaluate(() =>
      Array.from(document.scripts)
        .filter(script => script.nonce === '')
        .map(script => script.src || script.text.slice(0, 80))
    )

    expect(unnonced).toEqual([])
  })
})
