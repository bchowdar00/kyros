import { useEffect, useRef } from 'react'
import { BackIcon } from '@tiny/ui/icons/back-icon'
import { IconButton } from '@tiny/ui/components/icon-button'

export interface ChatPanelHeaderProps {
  onBack?: () => void
  subtitle?: string
  title?: string
}

export function ChatPanelHeader({
  onBack,
  subtitle = 'Start with a proven one or write your own below',
  title = 'Deploy an Automated Trading Strategy',
}: ChatPanelHeaderProps) {
  const heading = useRef<HTMLHeadingElement>(null)
  const wasChatting = useRef(false)
  useEffect(() => {
    if (wasChatting.current && !onBack) heading.current?.focus({ preventScroll: true })
    wasChatting.current = Boolean(onBack)
  }, [onBack])
  return (
    <header className="relative shrink-0 pt-lg">
      <div className="flex flex-col gap-copy pb-xl">
        <div className="flex min-h-touch items-center gap-copy">
          {onBack ? (
            <IconButton
              aria-label="Back to chats and public strategies"
              onClick={onBack}
              variant="ghost"
              className="shrink-0"
            >
              <BackIcon />
            </IconButton>
          ) : null}
          <h2
            ref={heading}
            tabIndex={-1}
            className="truncate text-heading-md font-regular text-foreground"
          >
            {title}
          </h2>
        </div>
        {!onBack ? (
          <p className="truncate text-label font-medium text-muted-foreground">{subtitle}</p>
        ) : null}
      </div>

      <div aria-hidden="true" className="absolute inset-x-0 bottom-0 h-px bg-border-subtle" />
    </header>
  )
}
