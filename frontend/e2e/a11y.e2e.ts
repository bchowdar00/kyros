import { AxeBuilder } from '@axe-core/playwright'
import { expect, test } from '@playwright/test'

import { gatedRoutes } from './routes'

// WCAG 2.2 AA is the merge gate (AGENTS.md §10). Runs in every device project so the phone
// and desktop layouts are both checked.
for (const route of gatedRoutes) {
  test(`${route.name} has no WCAG 2.2 AA violations`, async ({ page }) => {
    await page.goto(route.path)

    const results = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'])
      .analyze()

    const violations = results.violations.map(
      violation =>
        `${violation.id} (${violation.impact ?? 'unknown'}): ${violation.help}\n  ${violation.nodes
          .map(node => node.html)
          .join('\n  ')}`
    )

    expect(violations).toEqual([])
  })
}
