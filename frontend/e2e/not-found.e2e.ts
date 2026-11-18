import { expect, test } from '@playwright/test'

// A missing page must be a real 404, not a 200 with a noindex meta tag. The market segment
// layout resolves the market above the streaming boundary to make that true; Next injects
// `noindex` on 404 responses on its own, which the last assertion pins down.
test.describe('not found', () => {
  test('an unknown market answers 404 with the not-found screen', async ({ page }) => {
    const response = await page.goto('/market/does-not-exist')

    expect(response?.status()).toBe(404)
    await expect(page.getByRole('heading', { name: 'There is nothing here' })).toBeVisible()
    await expect(page.getByRole('link', { name: 'Go to the home page' })).toBeVisible()
  })

  test('an unknown path answers 404', async ({ page }) => {
    const response = await page.goto('/this-route-does-not-exist')

    expect(response?.status()).toBe(404)
    await expect(page.getByRole('heading', { name: 'There is nothing here' })).toBeVisible()
  })

  test('a planned nav destination answers a designed, unindexed 404', async ({ page }) => {
    const response = await page.goto('/leaderboard')

    expect(response?.status()).toBe(404)
    await expect(page.getByRole('heading', { name: 'There is nothing here' })).toBeVisible()
    await expect(page.getByRole('link', { name: 'Go to the home page' })).toBeVisible()
    await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', /noindex/)
  })
})
