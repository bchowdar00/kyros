import { expect, test } from '@playwright/test'

import { plannedDestinations, smokeRoutes } from './routes'

for (const route of smokeRoutes) {
  test(`${route.name} renders and hydrates without errors`, async ({ page }) => {
    const errors: string[] = []
    const notFound: string[] = []

    page.on('console', message => {
      if (message.type() !== 'error') {
        return
      }

      // Prefetches of planned destinations 404 until their routes exist; everything else
      // (a CSP block, a failed chunk, a runtime exception) fails the test.
      const pathname = new URL(message.location().url || 'http://placeholder/').pathname
      if (message.text().includes('404') && plannedDestinations.has(pathname)) {
        return
      }

      errors.push(`${message.text()} (${message.location().url})`)
    })
    page.on('pageerror', error => {
      errors.push(error.message)
    })
    page.on('response', response => {
      const pathname = new URL(response.url()).pathname

      if (response.status() === 404 && !plannedDestinations.has(pathname)) {
        notFound.push(pathname)
      }
    })

    const response = await page.goto(route.path)

    expect(response?.status()).toBe(200)
    await expect(page.getByRole('main')).toBeVisible()
    expect(errors).toEqual([])
    expect(notFound, 'resources that 404ed').toEqual([])
  })
}

test('every page carries the Tiny mark as its icon', async ({ page }) => {
  await page.goto('/')

  const href = await page.locator('link[rel="icon"]').first().getAttribute('href')

  expect(href, 'icon link').toBeTruthy()

  const icon = await page.request.get(href ?? '')

  expect(icon.status()).toBe(200)
  expect(icon.headers()['content-type']).toContain('image/svg+xml')
})
