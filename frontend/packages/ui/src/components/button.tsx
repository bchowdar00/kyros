import { Slot as SlotPrimitive } from 'radix-ui/slot'
import type { ComponentPropsWithRef } from 'react'
import type { VariantProps } from 'class-variance-authority'

import { buttonVariants } from './_button-styles'
import { cn } from '../lib/cn'

export { buttonVariants }
export type { ButtonSize, ButtonVariant } from './_button-styles'

export type ButtonProps = ComponentPropsWithRef<'button'> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean
  }

export function Button({ asChild = false, className, size, variant, ...props }: ButtonProps) {
  const classes = cn(buttonVariants({ size, variant }), className)

  if (asChild) {
    return <SlotPrimitive {...props} className={classes} />
  }

  return <button type="button" {...props} className={classes} />
}
