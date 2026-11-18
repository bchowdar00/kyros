'use client'

import { Button } from '@tiny/ui/components/button'

interface MarketErrorProps {
  error: Error & { digest?: string }
  retry: () => void
}

// The error itself is never shown: its message can carry internals. Next logs it with the
// digest on the server; a reporting vendor plugs in here when one is chosen.
export default function MarketError({ retry }: MarketErrorProps) {
  return (
    <main className="flex min-h-dvh flex-col items-center justify-center gap-lg px-xl pb-safe-bottom text-center">
      <h1 className="text-heading-md font-regular text-foreground">
        This screen ran into a problem
      </h1>
      <p className="max-w-prose text-body text-muted-foreground">
        Try again. If it keeps happening, reload the page.
      </p>
      <Button onClick={retry} size="large" variant="secondary">
        Try again
      </Button>
    </main>
  )
}
