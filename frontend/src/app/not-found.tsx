import { AppShell } from '@/components/app-shell/app-shell'
import { NotFoundScreen } from '@/components/not-found/not-found-screen'

// The root 404 answers URLs that match no segment, so it renders outside the `(app)`
// layout and has to bring the shell itself.
export default function NotFound() {
  return (
    <AppShell>
      <NotFoundScreen description="The link may be old, not built yet, or removed." />
    </AppShell>
  )
}
