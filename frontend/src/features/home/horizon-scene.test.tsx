import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import { expectNoA11yViolations } from '@kyros/testing/a11y'

import { HorizonScene } from './horizon-scene'

describe('HorizonScene', () => {
  it('is decoration: one canvas hidden from assistive tech that takes no clicks', async () => {
    const { container } = render(<HorizonScene />)

    const canvas = container.querySelector('canvas')
    expect(canvas).not.toBeNull()
    expect(canvas).toHaveAttribute('aria-hidden', 'true')
    expect(canvas?.className).toContain('pointer-events-none')
    await expectNoA11yViolations(container)
  })

  it('leaves the page standing where there is no WebGL to draw with', async () => {
    // jsdom has no WebGL: the scene must give up quietly, not throw.
    const { container, unmount } = render(<HorizonScene />)
    await new Promise(resolve => setTimeout(resolve, 50))

    expect(container.querySelector('canvas')).not.toBeNull()
    expect(() => unmount()).not.toThrow()
  })
})
