import { cva, type VariantProps } from 'class-variance-authority'
import type { ComponentPropsWithRef } from 'react'

import { cn } from '../lib/cn'

export type ScrollFadeEdge = 'bottom' | 'left' | 'right' | 'top'

const scrollFadeVariants = cva('pointer-events-none absolute z-10', {
  compoundVariants: [
    { class: 'from-background to-transparent', edge: 'top', strength: 'full' },
    { class: 'from-background/85 to-transparent', edge: 'top', strength: 'strong' },
    { class: 'from-transparent to-background', edge: 'right', strength: 'full' },
    { class: 'from-transparent to-background/85', edge: 'right', strength: 'strong' },
    { class: 'from-transparent to-background', edge: 'bottom', strength: 'full' },
    { class: 'from-transparent to-background/85', edge: 'bottom', strength: 'strong' },
    { class: 'from-background to-transparent', edge: 'left', strength: 'full' },
    { class: 'from-background/85 to-transparent', edge: 'left', strength: 'strong' },
  ],
  defaultVariants: { edge: 'right', strength: 'full' },
  variants: {
    edge: {
      bottom: 'inset-x-0 bottom-0 h-4xl bg-linear-to-b',
      left: 'inset-y-0 left-0 w-4xl bg-linear-to-r',
      right: 'inset-y-0 right-0 w-4xl bg-linear-to-r',
      top: 'inset-x-0 top-0 h-4xl bg-linear-to-b',
    },
    /** `strong` ends at 85% background instead of fully opaque, per the market screens. */
    strength: {
      full: '',
      strong: '',
    },
  },
})

export type ScrollFadeProps = Omit<ComponentPropsWithRef<'div'>, 'aria-hidden'> &
  VariantProps<typeof scrollFadeVariants>

export function ScrollFade({ className, edge, strength, ...props }: ScrollFadeProps) {
  return (
    <div
      {...props}
      aria-hidden="true"
      className={cn(scrollFadeVariants({ edge, strength }), className)}
    />
  )
}
