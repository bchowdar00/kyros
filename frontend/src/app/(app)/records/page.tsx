import { permanentRedirect } from 'next/navigation'

/** The page moved: links to Records made before it became the leaderboard still arrive. */
export default function RecordsPage(): never {
  permanentRedirect('/leaderboard')
}
