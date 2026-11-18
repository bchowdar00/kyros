import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import { ValueSlot } from '@/features/chat/widgets/value-slot'

describe('ValueSlot', () => {
  it('reserves the widest width it has shown so a shorter value does not shrink the row', () => {
    const { rerender } = render(<ValueSlot>$79,999.99</ValueSlot>)
    const slot = screen.getByText('$79,999.99')
    expect(slot.style.minWidth).toBe('10ch')

    rerender(<ValueSlot>$80,000</ValueSlot>)
    expect(screen.getByText('$80,000').style.minWidth).toBe('10ch')

    rerender(<ValueSlot>$100,000.00</ValueSlot>)
    expect(screen.getByText('$100,000.00').style.minWidth).toBe('11ch')
  })
})
