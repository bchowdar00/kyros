import { AxeBuilder } from '@axe-core/playwright'
import { expect, test } from '@playwright/test'

// Below the `lg` breakpoint every app route renders the mobile-coming-soon screen
// (AGENTS.md §0.2, desktop-first interim). The app shell must be gone from the
// accessibility tree, not just visually hidden.
test('a small viewport sees the open-on-a-laptop notice instead of the app', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })

  const response = await page.goto('/')

  expect(response?.status()).toBe(200)
  await expect(page.getByRole('heading', { name: 'Open Kyros on a laptop' })).toBeVisible()
  await expect(page.getByRole('navigation', { name: 'Primary' })).toBeHidden()

  const results = await new AxeBuilder({ page })
    .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'])
    .analyze()
  const violations = results.violations.map(
    violation => `${violation.id} (${violation.impact ?? 'unknown'}): ${violation.help}`
  )

  expect(violations).toEqual([])
})
