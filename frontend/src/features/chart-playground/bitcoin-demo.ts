import type { ChartPoint } from '@kyros/ui/charts/lib/types'

export const BITCOIN_SCENARIOS = [
  'steady',
  'rally',
  'drop',
  'recovery',
  'sparse',
  'disconnect',
  'rollover',
] as const
export type BitcoinScenario = (typeof BITCOIN_SCENARIOS)[number]

export const DEMO_TARGET_CENTS = 7_567_418
const TICK_MS = 250
const WINDOW_MS = 5 * 60_000
const MAX_POINTS = WINDOW_MS / TICK_MS + 1
const START = Date.UTC(2026, 4, 23, 3, 29)

export function demoPriceCents(scenario: BitcoinScenario, tick: number): number {
  const ordinary = Math.round(Math.sin(tick / 13) * 60 + Math.sin(tick / 3) * 12)
  const shock = tick >= 16 ? 24_000 : 0
  const recovery = Math.max(0, 24_000 - Math.max(0, tick - 32) * 600)
  const movement =
    scenario === 'rally'
      ? shock
      : scenario === 'drop'
        ? -shock
        : scenario === 'recovery' && tick >= 16
          ? -recovery
          : 0
  const rolloverDrift = scenario === 'rollover' ? Math.min(32, Math.max(0, tick)) * 100 : 0
  return DEMO_TARGET_CENTS + ordinary + movement + rolloverDrift
}

/** Explicit demo only. Values stay in cents until the existing chart coordinate boundary. */
function demoPoint(scenario: BitcoinScenario, tick: number): ChartPoint {
  return { time: START + tick * TICK_MS, value: demoPriceCents(scenario, tick) / 100 }
}

/** A bounded, deterministic streaming source shared with React through its snapshot contract. */
export function createBitcoinDemo(scenario: BitcoinScenario) {
  let tick = 0
  const historyLength = scenario === 'sparse' ? 4 : MAX_POINTS
  const initial = {
    disconnected: false,
    targetPriceCents: DEMO_TARGET_CENTS,
    marketEndsAt: START + 8_000,
    paused: false,
    points: Array.from({ length: historyLength }, (_, index) =>
      demoPoint(scenario, index - historyLength + 1)
    ),
    now: START,
  }
  let snapshot = initial
  const listeners = new Set<() => void>()
  const emit = () => {
    for (const listener of listeners) listener()
  }
  const advance = () => {
    if (snapshot.paused) return
    tick += 1
    const disconnected = scenario === 'disconnect' && tick >= 16 && tick < 40
    snapshot = {
      ...snapshot,
      disconnected,
      targetPriceCents:
        scenario === 'rollover' && tick >= 32 ? demoPriceCents(scenario, 32) : DEMO_TARGET_CENTS,
      marketEndsAt:
        scenario === 'rollover' && tick >= 32 ? START + 5 * 60_000 + 8_000 : START + 8_000,
      now: START + tick * TICK_MS,
      points: disconnected
        ? snapshot.points
        : [...snapshot.points, demoPoint(scenario, tick)].slice(-MAX_POINTS),
    }
    emit()
  }
  return {
    getSnapshot: () => snapshot,
    getServerSnapshot: () => initial,
    subscribe(listener: () => void) {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
    advance,
    start() {
      // Timer delivers ticks even with reduced motion; animation is a renderer concern.
      const timer = setInterval(() => {
        if (!document.hidden) advance()
      }, TICK_MS)
      return () => clearInterval(timer)
    },
    togglePause() {
      snapshot = { ...snapshot, paused: !snapshot.paused }
      emit()
    },
    replay() {
      tick = 0
      snapshot = initial
      emit()
    },
  }
}
