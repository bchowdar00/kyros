import { Button } from '@tiny/ui/components/button'
import Link from 'next/link'

interface NotFoundScreenProps {
  description: string
}

/**
 * Shared 404 screen. Next.js answers these with a real 404 status and injects
 * `<meta name="robots" content="noindex">` on its own, so dead links (including the
 * planned nav destinations) stay out of the index without extra wiring.
 */
export function NotFoundScreen({ description }: NotFoundScreenProps) {
  return (
    <main className="flex min-h-below-nav flex-col items-center justify-center gap-lg px-xl pb-safe-bottom text-center">
      <p aria-hidden="true" className="text-label font-medium tracking-label text-muted-foreground">
        404
      </p>
      <h1 className="text-2xl font-regular text-foreground">There is nothing here</h1>
      <p className="max-w-prose text-body text-muted-foreground">{description}</p>
      <Button asChild size="large" variant="secondary">
        <Link href="/">Go to the home page</Link>
      </Button>
    </main>
  )
}
