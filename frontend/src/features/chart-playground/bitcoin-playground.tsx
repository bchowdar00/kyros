'use client'

import { useBrowserTimeZone } from '@tiny/ui/lib/use-browser-time-zone'
import { useEffect, useState, useSyncExternalStore } from 'react'
import { Button } from '@tiny/ui/components/button'
import { PriceMarketCard } from '@tiny/ui/charts/price-market-card'

import { BITCOIN_SCENARIOS, type BitcoinScenario, createBitcoinDemo } from './bitcoin-demo'

const scenarioLabels: Record<BitcoinScenario, string> = {
  steady: 'Ordinary ticks',
  rally: 'Sharp rise',
  drop: 'Sharp drop',
  recovery: 'Drop and recovery',
  sparse: 'Short history',
  disconnect: 'Disconnect and reconnect',
  rollover: 'Market rollover',
}
const scenarioDescriptions: Record<BitcoinScenario, string> = {
  steady: 'Small ticks across the price to beat. The history should hold its shape.',
  rally: 'At 4 seconds, Bitcoin jumps $240. The scale and price to beat should move together.',
  drop: 'At 4 seconds, Bitcoin falls $240. The full move and price to beat should remain visible.',
  recovery:
    'A $240 drop at 4 seconds, followed by recovery from 8 seconds. Watch the scale settle.',
  sparse: 'Only one second of history is available. The chart starts at the first known sample.',
  rollover:
    'At 8 seconds, the next market begins. The target and timer change while the same price line keeps flowing.',
  disconnect:
    'The feed disconnects at 4 seconds and reconnects at 10. No ticks are invented during the gap.',
}

function BitcoinDemoChart({ scenario }: { scenario: BitcoinScenario }) {
  const [demo] = useState(() => createBitcoinDemo(scenario))
  const timeZone = useBrowserTimeZone()
  const snapshot = useSyncExternalStore(demo.subscribe, demo.getSnapshot, demo.getServerSnapshot)
  const [replay, setReplay] = useState(0)
  useEffect(() => demo.start(), [demo])
  const status = snapshot.disconnected ? 'Disconnected' : snapshot.paused ? 'Paused' : 'Playing'

  return (
    <>
      <div className="flex flex-wrap items-center gap-md">
        <output className="mr-auto text-label text-muted-foreground">Demo · {status}</output>
        <Button onClick={demo.togglePause} variant="secondary">
          {snapshot.paused ? 'Resume demo' : 'Pause demo'}
        </Button>
        <Button
          onClick={() => {
            demo.replay()
            setReplay(replay + 1)
          }}
          variant="secondary"
        >
          Replay demo
        </Button>
      </div>
      <PriceMarketCard
        asset={{ color: 'var(--asset-btc)', symbol: 'BTC' }}
        clock="sample"
        feed={{
          state: snapshot.disconnected ? 'disconnected' : snapshot.paused ? 'paused' : 'live',
        }}
        key={replay}
        points={snapshot.points}
        priceBand={60}
        targetLabel="Price to beat"
        targetPrice={snapshot.targetPriceCents / 100}
        timeZone={timeZone}
        title="Bitcoin Up or Down · Demo"
        windows={[{ candleInterval: 15_000, durationMs: 300_000 }]}
      />
    </>
  )
}

export function BitcoinPlayground() {
  const [scenario, setScenario] = useState<BitcoinScenario>('steady')
  return (
    <section aria-label="Bitcoin chart demos" className="flex flex-col gap-lg">
      <div className="flex flex-wrap items-center justify-between gap-lg">
        <div className="flex flex-col gap-xs">
          <h1 className="text-heading-md text-foreground">Bitcoin live chart lab</h1>
          <p className="text-label text-muted-foreground">
            Simulated data · repeatable scenarios · no live market prices
          </p>
        </div>

        <label className="flex items-center gap-md text-label text-foreground">
          Demo scenario
          <select
            className="min-h-touch rounded-md border border-border bg-surface-raised px-md text-body focus-visible:outline-2 focus-visible:outline-foreground"
            value={scenario}
            onChange={event => {
              const selected = BITCOIN_SCENARIOS.find(
                candidate => candidate === event.currentTarget.value
              )
              if (selected) setScenario(selected)
            }}
          >
            {BITCOIN_SCENARIOS.map(candidate => (
              <option key={candidate} value={candidate}>
                {scenarioLabels[candidate]}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="text-body text-muted-foreground">
        {scenarioDescriptions[scenario]} Hover and hold the pointer still to check the tooltip.
      </p>
      <BitcoinDemoChart key={scenario} scenario={scenario} />
    </section>
  )
}
