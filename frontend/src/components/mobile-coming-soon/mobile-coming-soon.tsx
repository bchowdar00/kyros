import { TinyMarkIcon } from '@tiny/ui/icons/tiny-mark-icon'

/**
 * Interim screen for viewports below `lg` while the product is desktop-first
 * (AGENTS.md §0.2). Rendered as a CSS-only branch in the app layout — both branches are
 * server-rendered and one is hidden, so there is no JS breakpoint detection.
 */
export function MobileComingSoon() {
  return (
    <main className="flex min-h-dvh flex-col items-center justify-center gap-lg px-xl pb-safe-bottom text-center">
      <TinyMarkIcon aria-hidden className="size-2xl text-foreground" />
      <h1 className="text-2xl font-regular text-foreground">Tiny is on desktop for now</h1>
      <p className="max-w-prose text-body text-muted-foreground">
        The mobile app is coming soon. Until then, open Tiny on a bigger screen to trade.
      </p>
    </main>
  )
}
