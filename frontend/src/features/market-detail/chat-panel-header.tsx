import { useEffect, useRef } from 'react'
import { BackIcon } from '@kyros/ui/icons/back-icon'
import { IconButton } from '@kyros/ui/components/icon-button'

import { StrategySentence } from './strategy-sentence'

export interface ChatPanelHeaderProps {
  /** The strategy writer is working, so the sentence cannot be sent yet. */
  busy?: boolean
  onBack?: () => void
  /** With no chat open the rail opens on the strategy sentence; this builds it. */
  onBuild?: (sentence: string) => void
  title?: string
}

export function ChatPanelHeader({
  busy = false,
  onBack,
  onBuild,
  title = 'Build a strategy',
}: ChatPanelHeaderProps) {
  const heading = useRef<HTMLHeadingElement>(null)
  const wasChatting = useRef(false)
  useEffect(() => {
    if (wasChatting.current && !onBack) heading.current?.focus({ preventScroll: true })
    wasChatting.current = Boolean(onBack)
  }, [onBack])
  return (
    <header className="relative shrink-0 pt-xl">
      <div className="flex flex-col gap-copy pb-xl">
        {onBuild !== undefined && !onBack ? (
          <StrategySentence busy={busy} headingRef={heading} onBuild={onBuild} />
        ) : (
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
              className="truncate font-display text-display-sm font-bold tracking-heading text-foreground outline-none"
              ref={heading}
              tabIndex={-1}
            >
              {title}
            </h2>
          </div>
        )}
      </div>

      <div aria-hidden="true" className="absolute inset-x-0 bottom-0 h-px bg-border-subtle" />
    </header>
  )
}
