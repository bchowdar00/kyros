import axe, { type RunOptions } from 'axe-core'
import { expect } from 'vitest'

/**
 * Rules that need a layout engine. jsdom has none, so they always pass or always fail
 * for the wrong reasons. Playwright runs them against a real browser instead.
 */
const rulesWithoutLayoutInJsdom: NonNullable<RunOptions['rules']> = {
  'color-contrast': { enabled: false },
  region: { enabled: false },
}

/** Fails the test with one line per violation, naming the offending markup. */
export async function expectNoA11yViolations(
  container: Element,
  options: RunOptions = {}
): Promise<void> {
  const results = await axe.run(container, {
    ...options,
    rules: { ...rulesWithoutLayoutInJsdom, ...options.rules },
  })

  const violations = results.violations.map(
    violation =>
      `${violation.id} (${violation.impact ?? 'unknown'}): ${violation.help}\n  ${violation.nodes
        .map(node => node.html)
        .join('\n  ')}`
  )

  expect(violations, 'axe-core violations').toEqual([])
}
