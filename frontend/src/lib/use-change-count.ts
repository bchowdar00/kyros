import { useState } from 'react'

/**
 * How many times a value has changed since it first arrived: 0 until it does. A component
 * uses it to light a number for a moment when it moves and to stay still on arrival.
 */
export function useChangeCount<T>(value: T): number {
  const [last, setLast] = useState(value)
  const [changes, setChanges] = useState(0)
  if (value !== last) {
    setLast(value)
    setChanges(changes + 1)
  }
  return changes
}
