'use client'

import { IconButton } from '@tiny/ui/components/icon-button'
import { BookmarkIcon } from '@tiny/ui/icons/bookmark-icon'
import { ShareArrowIcon } from '@tiny/ui/icons/share-arrow-icon'
import { useState } from 'react'

const iconButtonClasses =
  'size-lg rounded-none border-0 p-0 hover:bg-transparent hover:text-muted-foreground'

export function MarketHeaderActions() {
  const [favorite, setFavorite] = useState(false)
  const [shareStatus, setShareStatus] = useState('')

  async function copyMarketLink() {
    try {
      await navigator.clipboard.writeText(window.location.href)
      setShareStatus('Market link copied')
    } catch {
      setShareStatus('Could not copy the market link')
    }
  }

  return (
    <div className="flex shrink-0 items-center gap-xs text-muted-foreground">
      <IconButton
        aria-label="Share market"
        className={iconButtonClasses}
        onClick={copyMarketLink}
        title="Copy market link"
      >
        <ShareArrowIcon className="size-lg" />
      </IconButton>
      <IconButton
        aria-label="Favorite market"
        aria-pressed={favorite}
        className={iconButtonClasses}
        onClick={() => setFavorite(!favorite)}
        title="Favorite for this visit"
      >
        <BookmarkIcon className="size-lg" fill={favorite ? 'currentColor' : 'none'} />
      </IconButton>
      <output className="sr-only">{shareStatus}</output>
    </div>
  )
}
