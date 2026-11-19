'use client'

import { cn } from '@kyros/ui/lib/cn'
import { useState } from 'react'

export interface ValueSlotProps {
  /** The formatted value; a number in the mono cut. */
  children: string
  className?: string
  align?: 'start' | 'end'
}

/**
 * A live number that never reflows its row. The slot is as wide as the widest text it has
 * shown, in `ch` of the mono cut, and only ever grows: $79,999.99 ticking to $80,000.00
 * and back changes the glyphs, not the layout around them.
 */
export function ValueSlot({ children, className, align = 'end' }: ValueSlotProps) {
  // Derived during render, like a held chart domain: the slot ratchets and never shrinks.
  const [widest, setWidest] = useState(children.length)
  if (children.length > widest) setWidest(children.length)
  const chars = Math.max(widest, children.length)
  return (
    <span
      className={cn(
        'inline-block whitespace-nowrap font-mono',
        { 'text-right': align === 'end', 'text-left': align === 'start' },
        className
      )}
      style={{ minWidth: `${chars}ch` }}
    >
      {children}
    </span>
  )
}
