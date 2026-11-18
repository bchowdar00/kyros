import type { ReactNode } from 'react'

import { ValueSlot } from './value-slot'
import { WidgetFrame } from '../widget-frame'

export type GaugeTone = 'positive' | 'negative' | 'neutral' | 'warning' | 'action'

export interface GaugeZone {
  from: number
  to: number
  tone: GaugeTone
  label?: string
}

export interface GaugeProps {
  label: string
  title?: string
  meta?: ReactNode
  value: number
  min: number
  max: number
  format: (value: number) => string
  zones?: readonly GaugeZone[]
  /** One line under the track that says what the position means right now. */
  reading?: string
}

const ZONE_CLASS: Record<GaugeTone, string> = {
  positive: 'bg-positive/40',
  negative: 'bg-negative/40',
  neutral: 'bg-surface-raised',
  warning: 'bg-warning/40',
  action: 'bg-action/60',
}

function percent(value: number, min: number, max: number) {
  const span = max - min || 1
  return Math.min(100, Math.max(0, ((value - min) / span) * 100))
}

/** A single bounded scalar on a horizontal track. Not a chart and not a number: its own thing. */
export function Gauge({
  label,
  title,
  meta,
  value,
  min,
  max,
  format,
  zones = [],
  reading,
}: GaugeProps) {
  const position = percent(value, min, max)
  // A boundary value belongs to the zone it opens, so a threshold reads as reached.
  const zone = zones.find(
    (candidate, index) =>
      value >= candidate.from && (value < candidate.to || index === zones.length - 1)
  )

  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <div className="flex items-baseline justify-between gap-md">
        <ValueSlot align="start" className="text-heading-md text-foreground">
          {format(value)}
        </ValueSlot>
        {zone?.label ? (
          <span className="text-label font-medium text-muted-foreground">{zone.label}</span>
        ) : null}
      </div>
      <div
        // A native <meter> cannot show zones; the role carries the same semantics.
        // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
        role="meter"
        aria-label={title ?? label}
        aria-valuenow={value}
        aria-valuemin={min}
        aria-valuemax={max}
        aria-valuetext={`${format(value)}${zone?.label ? `, ${zone.label}` : ''}`}
        className="relative py-sm"
      >
        <div className="relative flex h-sm gap-px overflow-clip rounded-pill bg-surface">
          {zones.map(band => (
            <div
              key={`${band.from}-${band.to}`}
              className={`absolute inset-y-0 ${ZONE_CLASS[band.tone]}`}
              style={{
                left: `${percent(band.from, min, max)}%`,
                width: `${percent(band.to, min, max) - percent(band.from, min, max)}%`,
              }}
            />
          ))}
        </div>
        <div
          aria-hidden="true"
          className="absolute top-1/2 size-md -translate-x-1/2 -translate-y-1/2 rounded-pill border-2 border-background bg-foreground"
          style={{ left: `${position}%` }}
        />
      </div>
      <div className="relative h-lg font-mono text-caption text-muted-foreground">
        <span className="absolute left-0">{format(min)}</span>
        {zones
          .filter(band => band.label)
          .map(band => (
            <span
              key={`${band.from}-${band.to}`}
              className="absolute -translate-x-1/2 font-sans"
              style={{
                left: `${(percent(band.from, min, max) + percent(band.to, min, max)) / 2}%`,
              }}
            >
              {band.label}
            </span>
          ))}
        <span className="absolute right-0">{format(max)}</span>
      </div>
      {reading ? <p className="text-label text-muted-foreground">{reading}</p> : null}
    </WidgetFrame>
  )
}
