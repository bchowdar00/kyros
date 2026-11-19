import { render } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { SignalSparkline } from './signal-sparkline'

describe('SignalSparkline', () => {
  it('renders a decorative canvas for any number of points', async () => {
    const { container, rerender } = render(<SignalSparkline points={[]} />)
    expect(container.querySelector('canvas')).toHaveAttribute('aria-hidden', 'true')

    rerender(<SignalSparkline points={[{ time: 1, value: 2 }]} />)
    rerender(
      <SignalSparkline
        points={[
          { time: 1, value: 2 },
          { time: 2, value: 3 },
          { time: 3, value: 1 },
        ]}
      />
    )
    await expectNoA11yViolations(container)
  })
})
