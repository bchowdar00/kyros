'use client'

import { useId, useState } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface SliderReadout {
  id: string
  label: string
  value: string
  tone?: 'positive' | 'negative'
}

export interface ScenarioSliderProps {
  label: string
  title?: string
  /** What the person is dragging, for example "BTC move from here". */
  name: string
  min: number
  max: number
  step: number
  defaultValue: number
  format: (value: number) => string
  /** Values that follow the slider; recomputed on every change. */
  derive: (value: number) => readonly SliderReadout[]
  onChange: (value: number) => void
}

/** One variable to drag, with the numbers that depend on it updating in place. */
export function ScenarioSlider({
  label,
  title,
  name,
  min,
  max,
  step,
  defaultValue,
  format,
  derive,
  onChange,
}: ScenarioSliderProps) {
  const [value, setValue] = useState(defaultValue)
  const inputId = useId()
  const position = ((value - min) / (max - min || 1)) * 100
  const readouts = derive(value)

  return (
    <WidgetFrame
      label={label}
      {...(title === undefined ? {} : { title })}
      meta={<span className="font-mono text-foreground">{format(value)}</span>}
    >
      <div className="flex flex-col gap-sm">
        <label htmlFor={inputId} className="text-caption text-muted-foreground">
          {name}
        </label>
        <input
          id={inputId}
          type="range"
          min={min}
          max={max}
          step={step}
          value={value}
          onChange={event => {
            const next = Number(event.currentTarget.value)
            setValue(next)
            onChange(next)
          }}
          className="h-sm w-full cursor-pointer appearance-none rounded-pill bg-surface-raised focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-focus-ring [&::-moz-range-thumb]:size-lg [&::-moz-range-thumb]:rounded-pill [&::-moz-range-thumb]:border-2 [&::-moz-range-thumb]:border-background [&::-moz-range-thumb]:bg-foreground [&::-webkit-slider-thumb]:size-lg [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-pill [&::-webkit-slider-thumb]:border-2 [&::-webkit-slider-thumb]:border-background [&::-webkit-slider-thumb]:bg-foreground"
          style={{
            backgroundImage: `linear-gradient(to right, var(--color-foreground) ${position}%, transparent ${position}%)`,
          }}
        />
        <div className="flex justify-between font-mono text-caption text-muted-foreground">
          <span>{format(min)}</span>
          <span>{format(max)}</span>
        </div>
      </div>
      <dl className="grid grid-cols-2 gap-x-lg gap-y-md">
        {readouts.map(readout => (
          <div key={readout.id} className="flex flex-col gap-copy">
            <dt className="text-caption text-muted-foreground">{readout.label}</dt>
            <dd
              className={
                readout.tone === 'positive'
                  ? 'font-mono text-body text-positive'
                  : readout.tone === 'negative'
                    ? 'font-mono text-body text-negative'
                    : 'font-mono text-body text-foreground'
              }
            >
              {readout.value}
            </dd>
          </div>
        ))}
      </dl>
    </WidgetFrame>
  )
}
