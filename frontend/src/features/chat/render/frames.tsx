import { cn } from '@kyros/ui/lib/cn'
import type { ReactNode } from 'react'

import { WidgetFrame } from '../widget-frame'
import { ChartShell, type ChartShellWindow } from '../widgets/chart-shell'

/** The rows a chart widget will have, so its skeleton is exactly as tall as the chart. */
export interface ChartSkeletonShape {
  title?: string | undefined
  legend?: boolean
  windows?: readonly ChartShellWindow[] | undefined
}

/**
 * The frame a widget will fill while the tool input streams or the binding loads. Given
 * a chart shape it draws the chart shell's rows empty at their final height, so the reply
 * does not move when the data lands.
 */
export function WidgetSkeleton({ label, chart }: { label: string; chart?: ChartSkeletonShape }) {
  if (chart) {
    return (
      <ChartShell
        label={label}
        title={chart.title ?? null}
        loading
        legend={chart.legend ? [] : undefined}
        windows={chart.windows}
      />
    )
  }
  return (
    <WidgetFrame label={label} title={null} className="min-h-chat-widget-min" aria-busy="true">
      <p className="m-auto text-label text-muted-foreground">Preparing…</p>
    </WidgetFrame>
  )
}

/** A widget that cannot be drawn, saying why rather than showing a guess (AGENTS.md §0.5). */
export function WidgetUnavailable({ label, reason }: { label: string; reason: string }) {
  return (
    <WidgetFrame label={label} title={label}>
      <output className="text-label text-muted-foreground">{reason}</output>
    </WidgetFrame>
  )
}

/**
 * Freshness under a bound widget: a line when the feed dropped, and the same line's height
 * kept while live, so the text under a widget does not move when the feed comes and goes.
 * Every live number carries visible freshness (AGENTS.md §7).
 */
export function WidgetFreshness({
  status,
  children,
}: {
  status: 'live' | 'stale'
  children: ReactNode
}) {
  const live = status === 'live'
  return (
    <div className="flex flex-col gap-xs">
      {children}
      <output
        aria-hidden={live || undefined}
        className={cn('px-lg text-caption text-muted-foreground', { invisible: live })}
      >
        Feed dropped. Showing the last values received.
      </output>
    </div>
  )
}

/** Content the model wrote, marked so its numbers are never read as live. */
export function WidgetAuthored({ children }: { children: ReactNode }) {
  return (
    <div className="flex flex-col gap-xs">
      {children}
      <p className="px-lg text-caption text-muted-foreground">Written by Kyros, not live data.</p>
    </div>
  )
}
