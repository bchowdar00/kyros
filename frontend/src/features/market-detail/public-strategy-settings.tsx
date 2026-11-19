'use client'

import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { MiniInfoIcon } from '@kyros/ui/icons/mini-info-icon'
import { cn } from '@kyros/ui/lib/cn'
import { useId } from 'react'

interface PublicStrategySettingsProps {
  scopeLabel: string
  /** The dollars-and-cents draft the widget owns; deploy reads it from there. */
  allocation: string
  onAllocationChange: (allocation: string) => void
}

const allocationPattern = /^\d*(?:\.\d{0,2})?$/

interface MoneyFieldProps {
  label: string
  /** The accessible name; the visible label carries an icon. */
  name: string
  value: string
  onChange: (value: string) => void
  /** Compact rows for the fields inside a disclosure. */
  compact?: boolean
}

/** A dollar figure, kept to two decimals as the user types. */
export function MoneyField({ label, name, value, onChange, compact = false }: MoneyFieldProps) {
  const fieldId = useId()
  return (
    <div className="flex min-h-touch items-center justify-between gap-md py-md">
      <label
        htmlFor={fieldId}
        className={cn('flex items-center gap-copy', {
          'text-heading-sm': !compact,
          'text-label text-muted-foreground': compact,
        })}
      >
        {label} {compact ? null : <MiniInfoIcon className="text-muted-foreground" />}
      </label>
      <div className="flex items-center rounded-sm border border-highlight bg-surface-raised px-md py-copy font-mono text-label">
        <span aria-hidden="true">$</span>
        <input
          id={fieldId}
          aria-label={name}
          className="field-sizing-content min-w-3xl max-w-20 bg-transparent text-right outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
          inputMode="decimal"
          value={value}
          maxLength={16}
          onChange={event => {
            if (allocationPattern.test(event.currentTarget.value))
              onChange(event.currentTarget.value)
          }}
        />
      </div>
    </div>
  )
}

interface AllocationFieldProps {
  value: string
  onChange: (value: string) => void
}

/** Dollars the strategy is funded with. */
export function AllocationField({ value, onChange }: AllocationFieldProps) {
  return (
    <MoneyField
      label="Total Allocation"
      name="Total allocation in USD"
      value={value}
      onChange={onChange}
    />
  )
}

interface ScopeFieldProps {
  scopeLabel: string
  /** A strategy built on a market page trades that market and no other. */
  fixed?: boolean
}

export function ScopeField({ scopeLabel, fixed = false }: ScopeFieldProps) {
  const scopeId = useId()
  return (
    <div className="flex min-h-touch items-center justify-between gap-md py-md">
      <label htmlFor={scopeId} className="flex items-center gap-copy text-heading-sm">
        Scope <MiniInfoIcon className="text-muted-foreground" />
      </label>
      <div className="relative min-w-0 max-w-2/3">
        <select
          id={scopeId}
          disabled={fixed}
          className="max-w-full cursor-pointer appearance-none truncate rounded-sm border border-highlight bg-surface-raised py-copy pr-2xl pl-sm text-label text-foreground focus-visible:outline-2 focus-visible:outline-focus-ring disabled:cursor-default disabled:opacity-100"
        >
          <option className="bg-background">{scopeLabel}</option>
        </select>
        {fixed ? null : (
          <NanoChevronIcon className="pointer-events-none absolute right-xs top-1/2 -translate-y-1/2 text-foreground" />
        )}
      </div>
    </div>
  )
}

/** The public widget's settings: the allocation draft the widget owns, and the fixed scope. */
export function PublicStrategySettings({
  scopeLabel,
  allocation,
  onAllocationChange,
}: PublicStrategySettingsProps) {
  return (
    <div>
      <AllocationField value={allocation} onChange={onAllocationChange} />
      <ScopeField scopeLabel={scopeLabel} />
    </div>
  )
}
