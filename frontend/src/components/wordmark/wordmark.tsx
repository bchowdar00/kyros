import { cn } from '@kyros/ui/lib/cn'
import type { ComponentPropsWithRef } from 'react'

/** The name set in the display face. There is no separate mark: the word is the logo. */
export function Wordmark({ className, ...props }: ComponentPropsWithRef<'span'>) {
  return (
    <span
      {...props}
      className={cn(
        'font-display text-wordmark font-extrabold tracking-display text-foreground',
        className
      )}
    >
      Kyros
    </span>
  )
}
