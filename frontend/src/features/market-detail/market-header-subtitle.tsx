'use client'

import { useBrowserTimeZone } from '@tiny/ui/lib/use-browser-time-zone'
import type { TimeRange } from '@tiny/ui/charts/lib/types'

import { MarketHeaderActions } from './market-header-actions'

interface MarketHeaderSubtitleProps {
  marketSlug: string
  range: TimeRange
}

const TIME_WHITESPACE = /\s/g

const formatters = new Map<string, ReturnType<typeof createFormatters>>()

function createFormatters(timeZone: string) {
  const date = new Intl.DateTimeFormat('en-US', {
    month: 'long',
    day: 'numeric',
    timeZone,
  })
  const time = new Intl.DateTimeFormat('en-US', {
    hour: 'numeric',
    minute: '2-digit',
    timeZone,
  })
  const zone = new Intl.DateTimeFormat('en-US', {
    timeZone,
    timeZoneName: 'short',
  })

  return { date, time, zone }
}

export function MarketHeaderSubtitle({ marketSlug, range }: MarketHeaderSubtitleProps) {
  const timeZone = useBrowserTimeZone()
  let formatter = formatters.get(timeZone)
  if (!formatter) {
    formatter = createFormatters(timeZone)
    formatters.set(timeZone, formatter)
  }
  const date = formatter.date.formatRange(range.start, range.end)
  const time = formatter.time.formatRange(range.start, range.end).replaceAll(TIME_WHITESPACE, '')
  const zone = formatter.zone
    .formatToParts(range.end)
    .find(part => part.type === 'timeZoneName')?.value

  return (
    <div className="flex items-center gap-sm text-muted-foreground">
      <p className="text-label font-regular tracking-label">
        {date}, {time} {zone}
      </p>
      <MarketHeaderActions key={marketSlug} />
    </div>
  )
}
