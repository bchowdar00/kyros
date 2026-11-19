import { cn } from '../lib/cn'

const MAX_RECENT_RESULTS = 14
const FALLBACK_RESULTS = ['below', 'above', 'below', 'above', 'below'] as const

export type RecentMarketResult = 'above' | 'below'

export interface RecentMarketResultsProps {
  /** Ordered historical outcomes, oldest to newest. Missing results receive a decorative fallback. */
  results?: readonly RecentMarketResult[] | undefined
}

function resolveDisplayedResults(results: readonly RecentMarketResult[] | undefined) {
  const recentResults = results?.slice(-MAX_RECENT_RESULTS)

  return recentResults?.length ? recentResults : FALLBACK_RESULTS
}

/** A form line: one square per settled round, oldest first, lettered so colour never stands alone. */
export function RecentMarketResults({ results }: RecentMarketResultsProps) {
  const displayedResults = resolveDisplayedResults(results)
  const hasResults = results !== undefined && results.length > 0
  const listLabel = hasResults
    ? `Results for the ${displayedResults.length} most recent markets`
    : 'Recent market results unavailable'

  return (
    <ol aria-label={listLabel} className="flex flex-wrap items-center gap-px">
      {displayedResults.map((result, index) => {
        const resolvedAboveTarget = result === 'above'
        const markerLabel = hasResults
          ? `Market resolved ${resolvedAboveTarget ? 'above' : 'below'} the price to beat`
          : 'Market result unavailable'

        return (
          <li
            aria-label={markerLabel}
            className={cn(
              'flex size-xl items-center justify-center rounded-xs border-[1.5px] text-caption font-bold',
              !hasResults
                ? 'border-border-subtle text-muted-foreground'
                : resolvedAboveTarget
                  ? 'border-action bg-action text-on-action'
                  : 'border-foreground bg-foreground text-background'
            )}
            key={`${result}-${index}`}
          >
            <span aria-hidden="true">{hasResults ? (resolvedAboveTarget ? 'U' : 'D') : '·'}</span>
          </li>
        )
      })}
    </ol>
  )
}
