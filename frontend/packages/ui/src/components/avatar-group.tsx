import type { ComponentPropsWithRef } from 'react'

import { cn } from '../lib/cn'

export type AvatarGroupProps = ComponentPropsWithRef<'div'>

export function AvatarGroup({ className, ...props }: AvatarGroupProps) {
  return (
    <div
      {...props}
      className={cn('flex items-center -space-x-avatar-overlap', className)}
      role={props['aria-label'] === undefined ? undefined : 'group'}
    />
  )
}
