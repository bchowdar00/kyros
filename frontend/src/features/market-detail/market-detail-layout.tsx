import type { ReactNode } from 'react'

interface MarketDetailLayoutProps {
  marketColumn: ReactNode
  marketSlug: string
  strategyColumn?: ReactNode
}

/**
 * Centered two-column shell: the market column scrolls with the document while the rail
 * stays pinned below the nav at a viewport-bound height and scrolls internally.
 */
export function MarketDetailLayout({
  marketColumn,
  marketSlug,
  strategyColumn,
}: MarketDetailLayoutProps) {
  return (
    <main
      className={
        strategyColumn === undefined
          ? 'mx-auto grid w-full max-w-[90rem] grid-cols-1 px-4xl'
          : 'mx-auto grid w-full max-w-[90rem] grid-cols-[minmax(0,1fr)_var(--spacing-chat-rail)] gap-5xl px-4xl'
      }
      data-market-slug={marketSlug}
    >
      <section aria-label="Market details" className="min-w-0">
        {marketColumn}
      </section>

      {strategyColumn !== undefined ? (
        <aside aria-label="Chat panel" className="sticky top-app-nav h-below-nav min-h-0 py-xl">
          {strategyColumn}
        </aside>
      ) : null}
    </main>
  )
}
