import { cn } from '@kyros/ui/lib/cn'
import type { ComponentPropsWithRef, ReactNode } from 'react'

export interface WidgetFrameProps extends Omit<ComponentPropsWithRef<'section'>, 'title'> {
  /** Accessible name; a widget always says what it shows. */
  label: string
  /** Visible title in the header row. Defaults to the label. Pass `null` for no header. */
  title?: string | null
  /** Right-hand side of the header: a live value, a window, a count. */
  meta?: ReactNode
}

/**
 * The box every in-chat widget sits in. It is `chat-widget` wide, narrower than the
 * reply it sits in so it reads as an object placed in the conversation rather than a
 * panel, and it never exceeds its container so it shrinks with a narrower rail.
 *
 * One step raised from the rail behind it, hairline border, medium radius.
 */
export function WidgetFrame({
  label,
  title,
  meta,
  className,
  children,
  ...props
}: WidgetFrameProps) {
  const heading = title === undefined ? label : title
  return (
    <section
      {...props}
      aria-label={label}
      className={cn(
        'flex w-chat-widget max-w-full min-w-0 flex-col gap-md overflow-clip rounded-md border-[0.5px] border-border-subtle/50 bg-surface-subtle p-lg',
        className
      )}
    >
      {heading !== null ? (
        <header className="flex items-baseline justify-between gap-md">
          <h3 className="truncate text-label font-medium text-foreground">{heading}</h3>
          {meta !== undefined ? (
            <div className="shrink-0 text-label text-muted-foreground">{meta}</div>
          ) : null}
        </header>
      ) : null}
      {children}
    </section>
  )
}
