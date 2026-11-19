import { render, screen, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { ChatPlaygroundScreen } from '@/features/chat/chat-playground-screen'
import { GALLERY_ENTRY_COUNT, GALLERY_SECTIONS } from '@/features/chat/gallery-entries'

describe('ChatPlaygroundScreen', () => {
  it('renders one framed widget per gallery entry, grouped by section', async () => {
    const { container } = render(<ChatPlaygroundScreen />)
    let total = 0
    for (const section of GALLERY_SECTIONS) {
      const list = screen.getByRole('list', { name: `${section.title} widgets` })
      const frames = within(list).getAllByRole('region')
      expect(frames).toHaveLength(section.entries.length)
      total += frames.length
    }
    expect(total).toBe(GALLERY_ENTRY_COUNT)
    expect(GALLERY_ENTRY_COUNT).toBe(28)
    await expectNoA11yViolations(container)
  })
})
