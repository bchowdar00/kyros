'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useEffect, useState, useSyncExternalStore } from 'react'

import { type CalmSignals, getSignalsStream } from '@/features/exchange-streams/signals-stream'
import { useSignalsStream } from '@/features/exchange-streams/use-signals-stream'
import { pinnedSignalCards, signalFieldCards } from '@/features/signals/signal-cards'
import { applyFrames, type PinKey } from '@/features/signals/signal-catalogue'
import { SignalDetailDialog } from './signal-detail-dialog'
import { SignalsBrowser } from './signals-browser'
import { SignalsSection } from './signals-section'
import type { SignalsData } from './types'
import { usePinnedSignals } from './use-pinned-signals'
import { useSignalCatalogue } from './use-signal-catalogue'

export interface LiveSignalsSectionProps {
  apiUrl: string
  series: string
  /** The server-rendered strip, shown until the catalogue arrives or if it never does. */
  fallback: SignalsData
}

const CALM_EMPTY: CalmSignals = { frames: {}, history: {} }
const NO_HISTORY: readonly never[] = []

function ConnectedSignalsSection({ apiUrl, series, fallback }: LiveSignalsSectionProps) {
  const { data: catalogue } = useSignalCatalogue(apiUrl, series)
  const { pins, toggle } = usePinnedSignals(series)
  const [browsing, setBrowsing] = useState(false)
  // A strip card opened into its detail view, by pin so it follows the live value.
  const [detail, setDetail] = useState<{ pin: PinKey; trigger: HTMLElement }>()

  // Escape closes the strip's detail view and hands focus back to its card. The panel
  // handles its own Escape while it is open.
  useEffect(() => {
    if (detail === undefined || browsing) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      detail.trigger.focus()
      setDetail(undefined)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [detail, browsing])
  // The socket is shared with every chat widget on the page; this section asks for the
  // pinned signals, or everything while Browse All is open.
  const pinnedIds = [...new Set(pins.map(pin => pin.slice(0, pin.indexOf(':'))))]
  const { frames } = useSignalsStream(apiUrl, series, browsing ? 'series' : pinnedIds)
  const stream = getSignalsStream(apiUrl, series)
  // The panel reads the once-a-second snapshot so its rows settle; the strip stays live.
  const calm = useSyncExternalStore(stream.subscribeCalm, stream.getCalmSnapshot, () => CALM_EMPTY)

  if (catalogue === undefined || catalogue === null) {
    return <SignalsSection {...fallback} />
  }

  const live = applyFrames(catalogue, frames)
  const detailCard =
    detail === undefined
      ? undefined
      : signalFieldCards(live, { showAll: true }).find(card => card.pin === detail.pin)
  return (
    <>
      <SignalsSection
        browseLabel={fallback.browseLabel}
        items={pinnedSignalCards(live, pins)}
        onBrowse={() => setBrowsing(true)}
        // A live strip card's id is the pin it was built from (`pinnedSignalCards`).
        onOpen={(id, trigger) => setDetail({ pin: id as PinKey, trigger })}
        title={fallback.title}
        total={signalFieldCards(live).length}
      />
      {detailCard !== undefined && !browsing ? (
        <SignalDetailDialog
          card={detailCard}
          history={calm.history[detailCard.pin] ?? NO_HISTORY}
          onClose={() => {
            detail?.trigger.focus()
            setDetail(undefined)
          }}
          onToggle={toggle}
          pinned={pins.includes(detailCard.pin)}
          placement="viewport"
        />
      ) : null}
      {browsing ? (
        <SignalsBrowser
          catalogue={applyFrames(catalogue, calm.frames)}
          history={calm.history}
          onClose={() => setBrowsing(false)}
          onTogglePin={toggle}
          pins={pins}
        />
      ) : null}
    </>
  )
}

/** The signals strip fed by the exchange's catalogue and socket, with pinning and Browse All. */
export function LiveSignalsSection(props: LiveSignalsSectionProps) {
  const [queryClient] = useState(() => new QueryClient())

  return (
    <QueryClientProvider client={queryClient}>
      <ConnectedSignalsSection {...props} />
    </QueryClientProvider>
  )
}
