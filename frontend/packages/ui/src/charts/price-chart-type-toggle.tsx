'use client'

import { useId } from 'react'
import { BitcoinIcon } from '../icons/bitcoin-icon'

export type PriceChartToggleKind = 'price' | 'probability'

export interface PriceChartTypeToggleProps {
  assetSymbol: string
  onChange: (kind: PriceChartToggleKind) => void
  value: PriceChartToggleKind
}

/** Native radios provide the keyboard selection contract without a toggle-group runtime. */
export function PriceChartTypeToggle({ assetSymbol, onChange, value }: PriceChartTypeToggleProps) {
  const name = useId()
  return (
    <fieldset
      aria-label={`${assetSymbol} chart data`}
      className="flex items-center gap-md rounded-md border border-border-subtle bg-surface-subtle p-xs"
    >
      {(['price', 'probability'] as const).map(kind => (
        <label
          key={kind}
          className="relative flex size-3xl after:absolute after:left-1/2 after:top-1/2 after:min-h-touch after:min-w-touch after:-translate-x-1/2 after:-translate-y-1/2 cursor-pointer items-center justify-center rounded-sm text-label font-medium text-foreground has-checked:bg-foreground has-checked:text-background has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-focus-ring"
        >
          <input
            aria-label={
              kind === 'price' ? `${assetSymbol} price chart` : 'Market probability chart'
            }
            checked={value === kind}
            className="sr-only"
            name={name}
            onChange={() => onChange(kind)}
            type="radio"
            value={kind}
          />
          {kind === 'probability' ? (
            <span aria-hidden="true">%</span>
          ) : assetSymbol === 'BTC' ? (
            <BitcoinIcon className="size-3xl" />
          ) : (
            <span aria-hidden="true">{assetSymbol}</span>
          )}
        </label>
      ))}
    </fieldset>
  )
}
