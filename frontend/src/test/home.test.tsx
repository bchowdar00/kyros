import { render, screen } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { HomeScreen } from '@/features/home/home-screen'

vi.mock('next/navigation', () => ({ useRouter: () => ({ refresh: vi.fn() }) }))

describe('home page', () => {
  it('does not render the prototype fixture markets', async () => {
    const { container } = render(<HomeScreen cards={[]} />)

    expect(screen.queryByRole('link', { name: 'Celtics at Lakers' })).not.toBeInTheDocument()
    expect(screen.queryByRole('link', { name: 'BTC above $118K' })).not.toBeInTheDocument()
    expect(screen.getByRole('heading', { name: 'Markets' })).toBeInTheDocument()
    expect(screen.getByText('No live markets right now.')).toBeInTheDocument()

    await expectNoA11yViolations(container)
  })
})
