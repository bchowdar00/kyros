'use client'

import './globals.css'

interface GlobalErrorProps {
  error: Error & { digest?: string }
  retry: () => void
}

// Replaces the root layout when it throws, so nothing from the app tree (fonts, providers)
// can be relied upon. Plain markup, plain button, one action.
export default function GlobalError({ retry }: GlobalErrorProps) {
  return (
    <html lang="en">
      <body className="bg-background text-foreground">
        <main className="flex min-h-dvh flex-col items-center justify-center gap-lg px-xl text-center">
          <h1 className="text-heading-md font-regular">This screen ran into a problem</h1>
          <p className="max-w-prose text-body text-muted-foreground">
            Try again. If it keeps happening, reload the page.
          </p>
          <button
            className="min-h-touch rounded-xs border border-border bg-surface px-xl text-body text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
            onClick={retry}
            type="button"
          >
            Try again
          </button>
        </main>
      </body>
    </html>
  )
}
