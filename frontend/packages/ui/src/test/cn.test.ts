import { describe, expect, it } from 'vitest'

import { cn } from '../lib/cn'

describe('cn', () => {
  it('resolves conflicting Tiny design-token utilities', () => {
    expect(cn('p-sm text-body font-regular', 'p-lg text-heading-sm font-medium')).toBe(
      'p-lg text-heading-sm font-medium'
    )
  })
})
