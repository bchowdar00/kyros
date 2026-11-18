import { useSyncExternalStore } from 'react'

export type HoverSource = 'keyboard' | 'pointer'

export interface HoverState {
  /** Index of the hovered sample in the primary series or candle list. */
  index: number
  source: HoverSource
  /** Snapped sample time. */
  time: number
  /** Snapped x position in SVG pixels. */
  x: number
}

export interface HoverStore {
  get(): HoverState | null
  set(next: HoverState | null): void
  subscribe(listener: () => void): () => void
}

function isSameHover(a: HoverState | null, b: HoverState | null) {
  if (a === b) {
    return true
  }

  if (!a || !b) {
    return false
  }

  return a.index === b.index && a.time === b.time && a.x === b.x && a.source === b.source
}

/**
 * Minimal external store so only hover-aware parts of a chart re-render while
 * the pointer moves; series paths stay untouched.
 */
export function createHoverStore(): HoverStore {
  let state: HoverState | null = null
  const listeners = new Set<() => void>()

  return {
    get: () => state,
    set: next => {
      if (isSameHover(state, next)) {
        return
      }

      state = next

      for (const listener of listeners) {
        listener()
      }
    },
    subscribe: listener => {
      listeners.add(listener)

      return () => {
        listeners.delete(listener)
      }
    },
  }
}

const getServerSnapshot = () => null

export function useHoverState(store: HoverStore) {
  return useSyncExternalStore(store.subscribe, store.get, getServerSnapshot)
}
