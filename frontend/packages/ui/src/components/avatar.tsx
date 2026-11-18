import * as AvatarPrimitive from 'radix-ui/avatar'
import type { ComponentPropsWithRef, ReactNode } from 'react'

import { cn } from '../lib/cn'

export type AvatarSize = 'large' | 'medium' | 'small'

export type AvatarProps = Omit<
  ComponentPropsWithRef<typeof AvatarPrimitive.Root>,
  'asChild' | 'children'
> & {
  alt: string
  fallback?: ReactNode
  fallbackDelayMs?: number
  framed?: boolean
  imageClassName?: string
  size?: AvatarSize
  src?: string
}

const sizeClasses: Record<AvatarSize, string> = {
  small: 'size-avatar-sm',
  medium: 'size-avatar-md',
  large: 'size-avatar-lg',
}

export function Avatar({
  alt,
  className,
  fallback,
  fallbackDelayMs,
  framed = false,
  imageClassName,
  size = 'medium',
  src,
  ...props
}: AvatarProps) {
  const accessibleLabel = props['aria-label'] ?? (alt.length > 0 ? alt : undefined)

  return (
    <AvatarPrimitive.Root
      {...props}
      aria-label={accessibleLabel}
      className={cn(
        'inline-flex shrink-0 items-center justify-center overflow-hidden rounded-pill bg-surface-raised',
        framed && 'border border-highlight',
        sizeClasses[size],
        className
      )}
      role={accessibleLabel === undefined ? undefined : 'img'}
    >
      <AvatarPrimitive.Image
        alt=""
        className={cn(
          'rounded-pill object-cover',
          framed ? 'size-[calc(100%_-_var(--space-1))]' : 'size-full',
          imageClassName
        )}
        draggable={false}
        src={src}
      />
      <AvatarPrimitive.Fallback
        {...(fallbackDelayMs === undefined ? {} : { delayMs: fallbackDelayMs })}
        aria-hidden={accessibleLabel === undefined ? undefined : true}
        className="flex size-full items-center justify-center text-label font-medium text-muted-foreground"
      >
        {fallback}
      </AvatarPrimitive.Fallback>
    </AvatarPrimitive.Root>
  )
}
