import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MobileComingSoon } from '@/components/mobile-coming-soon/mobile-coming-soon'

describe('MobileComingSoon', () => {
  it('says what Kyros is and where to open it', async () => {
    const { container } = render(<MobileComingSoon />)

    expect(screen.getByRole('heading', { name: 'Open Kyros on a laptop' })).toBeInTheDocument()
    expect(
      screen.getByText(/pretend money on Gemini's live prediction markets/i)
    ).toBeInTheDocument()
    expect(screen.getByText(/no sign-up and nothing to install/i)).toBeInTheDocument()

    await expectNoA11yViolations(container)
  })

  it('promises no mobile app', () => {
    render(<MobileComingSoon />)

    expect(screen.queryByText(/coming soon/i)).not.toBeInTheDocument()
  })
})
