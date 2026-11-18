import type { ComponentPropsWithRef, ReactNode } from 'react'

import { cn } from '../lib/cn'

export type SectionHeaderProps = ComponentPropsWithRef<'div'> & {
  action?: ReactNode
  headingLevel?: 2 | 3
  title: ReactNode
}

export function SectionHeader({
  action,
  className,
  headingLevel = 2,
  title,
  ...props
}: SectionHeaderProps) {
  const Heading = headingLevel === 2 ? 'h2' : 'h3'

  return (
    <div {...props} className={cn('flex items-center justify-between gap-lg', className)}>
      <Heading className="text-heading-sm font-regular text-foreground">{title}</Heading>
      {action}
    </div>
  )
}
