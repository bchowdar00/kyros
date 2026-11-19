import { Wordmark } from '@/components/wordmark/wordmark'

/**
 * Interim screen for viewports below `lg` while the product is desktop-first
 * (AGENTS.md §0.2). Rendered as a CSS-only branch in the app layout — both branches are
 * server-rendered and one is hidden, so there is no JS breakpoint detection.
 */
export function MobileComingSoon() {
  return (
    <main className="flex min-h-dvh flex-col items-center justify-center gap-lg px-xl pb-safe-bottom text-center">
      <Wordmark aria-hidden="true" />
      <h1 className="font-display text-display-md font-bold tracking-display text-foreground">
        Kyros is on desktop for now
      </h1>
      <p className="max-w-prose text-body text-muted-foreground">
        The mobile app is coming soon. Until then, open Kyros on a bigger screen to trade.
      </p>
    </main>
  )
}
