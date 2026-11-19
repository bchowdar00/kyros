'use client'

import { NanoChevronIcon } from '@kyros/ui/icons/nano-chevron-icon'
import { useId, useState } from 'react'

import { PublicStrategyCard, type PublicStrategyAction } from './public-strategy-card'
import type { PublicStrategy, PublicStrategySortId, PublicStrategySortOption } from './types'

export interface PublicStrategiesListProps {
  marketKey?: string
  defaultSortId?: PublicStrategySortId
  onRunStrategy?: PublicStrategyAction
  onInverseStrategy?: PublicStrategyAction
  availability?: 'ready' | 'unavailable'
  sectionTitle?: string
  sortLabel?: string
  sortOptions?: readonly PublicStrategySortOption[]
  strategies: readonly PublicStrategy[]
}

function sortStrategies(
  strategies: readonly PublicStrategy[],
  sortId: PublicStrategySortId
): PublicStrategy[] {
  const getSortValue = (strategy: PublicStrategy) =>
    sortId === 'most-deployed' ? strategy.deployedCents : strategy.runningCount

  return strategies.toSorted((first, second) => getSortValue(second) - getSortValue(first))
}

export function PublicStrategiesList({
  marketKey = 'standalone-chat',
  defaultSortId = 'most-running',
  onRunStrategy,
  onInverseStrategy,
  availability = 'ready',
  sectionTitle = 'Public Strategies',
  sortLabel = 'Sort',
  sortOptions = [
    { id: 'most-running', label: 'Most Running' },
    { id: 'most-deployed', label: 'Most Deployed' },
  ],
  strategies,
}: PublicStrategiesListProps) {
  const headingId = useId()
  const [sortId, setSortId] = useState<PublicStrategySortId>(defaultSortId)
  const sortedStrategies = sortStrategies(strategies, sortId)

  return (
    <section aria-labelledby={headingId} className="flex flex-col gap-lg">
      <div className="flex items-center justify-between">
        <h2
          className="font-display text-display-sm font-bold tracking-heading text-foreground"
          id={headingId}
        >
          {sectionTitle}
        </h2>

        <label className="flex items-center gap-xs text-label font-medium">
          <span className="text-muted-foreground">{sortLabel}:</span>
          <span className="relative flex items-center">
            <select
              disabled={strategies.length === 0}
              aria-label={`${sortLabel} public strategies`}
              className="-my-sort-hit min-h-touch cursor-pointer appearance-none bg-transparent pr-lg text-foreground outline-none focus-visible:rounded-xs focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
              onChange={event => setSortId(event.currentTarget.value as PublicStrategySortId)}
              value={sortId}
            >
              {sortOptions.map(option => (
                <option className="bg-background text-foreground" key={option.id} value={option.id}>
                  {option.label}
                </option>
              ))}
            </select>
            <NanoChevronIcon className="pointer-events-none absolute right-0 text-foreground" />
          </span>
        </label>
      </div>

      {strategies.length === 0 ? (
        <output className="w-fit rounded-sm border-[1.5px] border-dashed border-muted-foreground px-md py-sm text-body font-semibold text-foreground">
          {availability === 'unavailable'
            ? 'No public strategies to display.'
            : 'No public strategies yet.'}
        </output>
      ) : null}
      <div className="flex flex-col border-t border-border-subtle pt-xl">
        {sortedStrategies.map(strategy => (
          <PublicStrategyCard
            marketKey={marketKey}
            key={strategy.id}
            {...(onRunStrategy === undefined ? {} : { onRun: onRunStrategy })}
            {...(onInverseStrategy === undefined ? {} : { onInverse: onInverseStrategy })}
            strategy={strategy}
          />
        ))}
      </div>
    </section>
  )
}
