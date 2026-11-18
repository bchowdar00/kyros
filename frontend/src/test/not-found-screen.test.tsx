import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import { describe, expect, it } from 'vitest'

import { NotFoundScreen } from '@/components/not-found/not-found-screen'

describe('NotFoundScreen', () => {
  it('names the miss and offers a way home', async () => {
    const { container } = render(
      <NotFoundScreen description="This market may have ended, moved, or never existed." />
    )

    expect(screen.getByRole('heading', { name: 'There is nothing here' })).toBeInTheDocument()
    expect(
      screen.getByText('This market may have ended, moved, or never existed.')
    ).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Go to the home page' })).toHaveAttribute('href', '/')

    await expectNoA11yViolations(container)
  })
})
