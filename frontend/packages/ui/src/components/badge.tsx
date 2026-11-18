import { cva, type VariantProps } from 'class-variance-authority'
import type { ComponentPropsWithRef } from 'react'

import { cn } from '../lib/cn'

export const badgeVariants = cva(
  'inline-flex h-2xl shrink-0 items-center justify-center rounded-xs border px-md text-label font-regular tracking-label',
  {
    variants: {
      variant: {
        neutral: 'border-transparent bg-surface text-muted-foreground',
        positive: 'border-positive/20 bg-positive-muted text-positive',
        negative: 'border-negative/50 bg-negative/50 text-foreground',
        warning: 'border-warning/50 bg-warning/20 text-warning',
        action: 'border-action bg-action/80 text-foreground',
      },
    },
    defaultVariants: {
      variant: 'neutral',
    },
  }
)

export type BadgeVariant = NonNullable<VariantProps<typeof badgeVariants>['variant']>

export type BadgeProps = ComponentPropsWithRef<'span'> & VariantProps<typeof badgeVariants>

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span {...props} className={cn(badgeVariants({ variant }), className)} />
}
