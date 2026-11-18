'use client'

import { useEffect, useState } from 'react'

const SECOND_MS = 1000
const SECONDS_PER_MINUTE = 60

/** Market time keeps advancing even when the price feed is paused or disconnected. */
export function MarketCountdown({ endsAt }: { endsAt: number }) {
  const [now, setNow] = useState<number | null>(null)

  useEffect(() => {
    const update = () => setNow(Date.now())
    update()
    const interval = window.setInterval(update, SECOND_MS)
    return () => window.clearInterval(interval)
  }, [])

  if (now === null) return <>—</>
  const totalSeconds = Math.max(0, Math.ceil((endsAt - now) / SECOND_MS))
  const minutes = Math.floor(totalSeconds / SECONDS_PER_MINUTE)
  const seconds = totalSeconds % SECONDS_PER_MINUTE
  return <>{`${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`}</>
}
