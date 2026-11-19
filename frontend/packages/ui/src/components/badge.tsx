import { cva, type VariantProps } from 'class-variance-authority'
import type { ComponentPropsWithRef } from 'react'

import { cn } from '../lib/cn'

export const badgeVariants = cva(
  'inline-flex h-2xl shrink-0 items-center justify-center rounded-sm border-[1.5px] px-md text-label font-bold',
  {
    variants: {
      variant: {
        neutral: 'border-border-subtle bg-transparent text-muted-foreground',
        positive: 'border-positive bg-transparent text-positive',
        negative: 'border-foreground bg-foreground text-background',
        warning: 'border-dashed border-muted-foreground bg-transparent text-foreground',
        action: 'border-action bg-action text-on-action',
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
