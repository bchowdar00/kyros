import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { MobileComingSoon } from '@/components/mobile-coming-soon/mobile-coming-soon'

describe('MobileComingSoon', () => {
  it('explains that the product is desktop-only for now', async () => {
    const { container } = render(<MobileComingSoon />)

    expect(screen.getByRole('heading', { name: 'Kyros is on desktop for now' })).toBeInTheDocument()
    expect(screen.getByText(/mobile app is coming soon/i)).toBeInTheDocument()

    await expectNoA11yViolations(container)
  })
})
