import { Slot as SlotPrimitive } from 'radix-ui/slot'
import type { VariantProps } from 'class-variance-authority'
import type { ComponentPropsWithRef, ReactNode } from 'react'

import { iconButtonVariants } from './_button-styles'
import { cn } from '../lib/cn'

export { iconButtonVariants }

export type IconButtonProps = Omit<ComponentPropsWithRef<'button'>, 'aria-label' | 'children'> &
  VariantProps<typeof iconButtonVariants> & {
    'aria-label': string
    asChild?: boolean
    children: ReactNode
  }

export function IconButton({
  asChild = false,
  className,
  size,
  variant,
  ...props
}: IconButtonProps) {
  const classes = cn(iconButtonVariants({ size, variant }), className)

  if (asChild) {
    return <SlotPrimitive {...props} className={classes} />
  }

  return <button type="button" {...props} className={classes} />
}
