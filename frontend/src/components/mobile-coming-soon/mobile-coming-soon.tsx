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
        Open Kyros on a laptop
      </h1>
      <p className="max-w-prose text-body text-muted-foreground">
        Say a trading idea in one sentence. Kyros runs it with pretend money on Gemini&apos;s live
        prediction markets, Bitcoin and NBA, and keeps a public record of every round it trades.
      </p>
      <p className="max-w-prose text-body text-muted-foreground">
        The trading screen needs more room than this one has. Open this page on a laptop or desktop
        to try it: no sign-up and nothing to install.
      </p>
    </main>
  )
}
