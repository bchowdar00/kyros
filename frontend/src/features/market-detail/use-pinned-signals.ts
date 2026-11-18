'use client'

import { useSyncExternalStore } from 'react'

import { defaultPins, type PinKey } from '@/features/signals/signal-catalogue'

function storageKey(series: string): string {
  return `tiny.signal-pins.${series}`
}

function isPinKey(candidate: unknown): candidate is PinKey {
  return typeof candidate === 'string' && candidate.includes(':')
}

function readPins(series: string): PinKey[] | undefined {
  try {
    const raw = window.localStorage.getItem(storageKey(series))
    if (raw === null) return undefined
    const parsed: unknown = JSON.parse(raw)
    return Array.isArray(parsed) && parsed.every(isPinKey) ? parsed : undefined
  } catch {
    return undefined
  }
}

function writePins(series: string, pins: readonly PinKey[]) {
  try {
    window.localStorage.setItem(storageKey(series), JSON.stringify(pins))
  } catch {
    // Storage unavailable (private mode, quota): pins live for the page only.
  }
}

/**
 * One in-memory copy per series, so every subscriber sees the same array instance and
 * `useSyncExternalStore` does not re-render on identical snapshots. The server snapshot is
 * the default strip, which is what the page hydrates with before storage is consulted.
 */
const snapshots = new Map<string, PinKey[]>()
const defaults = new Map<string, PinKey[]>()
const listeners = new Set<() => void>()

function serverSnapshot(series: string): PinKey[] {
  let pins = defaults.get(series)
  if (pins === undefined) {
    pins = defaultPins(series)
    defaults.set(series, pins)
  }
  return pins
}

function clientSnapshot(series: string): PinKey[] {
  let pins = snapshots.get(series)
  if (pins === undefined) {
    pins = readPins(series) ?? serverSnapshot(series)
    snapshots.set(series, pins)
  }
  return pins
}

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

function setPins(series: string, pins: PinKey[]) {
  snapshots.set(series, pins)
  writePins(series, pins)
  for (const listener of listeners) listener()
}

/**
 * The fields pinned to the signals strip for one series. Starts from the round stats,
 * then whatever the browser remembered; persisted per series in `localStorage`.
 */
export function usePinnedSignals(series: string) {
  const pins = useSyncExternalStore(
    subscribe,
    () => clientSnapshot(series),
    () => serverSnapshot(series)
  )

  const toggle = (pin: PinKey) => {
    const current = clientSnapshot(series)
    setPins(series, current.includes(pin) ? current.filter(p => p !== pin) : [...current, pin])
  }

  return { pins, toggle }
}
