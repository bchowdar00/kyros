import { useEffect, useState, type RefObject } from 'react'

export interface ElementSize {
  height: number
  width: number
}

/** Tracks the content box of `ref` with a ResizeObserver; `null` until measured. */
export function useElementSize(
  ref: RefObject<HTMLElement | null>,
  enabled: boolean
): ElementSize | null {
  const [size, setSize] = useState<ElementSize | null>(null)

  useEffect(() => {
    const element = ref.current

    if (!enabled || !element) {
      return
    }

    const update = (width: number, height: number) => {
      const next = { height: Math.round(height), width: Math.round(width) }

      setSize(previous =>
        previous && previous.width === next.width && previous.height === next.height
          ? previous
          : next
      )
    }

    const rect = element.getBoundingClientRect()
    update(rect.width, rect.height)

    if (typeof ResizeObserver === 'undefined') {
      return
    }

    const observer = new ResizeObserver(entries => {
      const entry = entries[0]

      if (!entry) {
        return
      }

      const box = entry.contentBoxSize?.[0]

      if (box) {
        update(box.inlineSize, box.blockSize)
      } else {
        update(entry.contentRect.width, entry.contentRect.height)
      }
    })

    observer.observe(element)

    return () => {
      observer.disconnect()
    }
  }, [enabled, ref])

  return size
}
