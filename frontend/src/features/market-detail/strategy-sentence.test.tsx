import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it, vi } from 'vitest'

import { StrategySentence } from './strategy-sentence'

describe('StrategySentence', () => {
  it('opens on a complete sentence and builds exactly what it says', async () => {
    const onBuild = vi.fn()
    const { container } = render(<StrategySentence onBuild={onBuild} />)

    expect(
      screen.getByRole('heading', {
        name: 'Buy Up when BTC is 10 basis points above the strike with two minutes left.',
      })
    ).toBeVisible()
    await userEvent.click(screen.getByRole('button', { name: 'Build this strategy' }))

    expect(onBuild).toHaveBeenCalledExactlyOnceWith(
      'Buy Up when BTC is 10 basis points above the strike with two minutes left.'
    )
    await expectNoA11yViolations(container)
  })

  it('turns the whole sentence around when the side changes', async () => {
    const onBuild = vi.fn()
    render(<StrategySentence onBuild={onBuild} />)

    await userEvent.click(screen.getByRole('button', { name: 'Up' }))
    await userEvent.click(screen.getByRole('button', { name: 'Build this strategy' }))

    expect(onBuild).toHaveBeenCalledExactlyOnceWith(
      'Buy Down when BTC is 10 basis points below the strike with two minutes left.'
    )
  })

  it('steps through the distances and the times, and comes back round', async () => {
    const onBuild = vi.fn()
    render(<StrategySentence onBuild={onBuild} />)

    await userEvent.click(screen.getByRole('button', { name: '10 basis points' }))
    await userEvent.click(screen.getByRole('button', { name: 'two minutes left' }))
    await userEvent.click(screen.getByRole('button', { name: 'Build this strategy' }))
    expect(onBuild).toHaveBeenLastCalledWith(
      'Buy Up when BTC is 20 basis points above the strike with one minute left.'
    )

    await userEvent.click(screen.getByRole('button', { name: '20 basis points' }))
    await userEvent.click(screen.getByRole('button', { name: '5 basis points' }))
    expect(screen.getByRole('button', { name: '10 basis points' })).toBeVisible()
  })

  it('cannot be built while the writer is busy', () => {
    render(<StrategySentence busy onBuild={vi.fn()} />)

    expect(screen.getByRole('button', { name: 'Build this strategy' })).toBeDisabled()
  })
})
