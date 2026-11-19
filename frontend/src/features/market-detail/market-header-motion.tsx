'use client'

import { motion, useReducedMotion, useScroll, useTransform } from 'motion/react'
import type { ReactNode } from 'react'

interface MarketHeaderMotionProps {
  actions: ReactNode
  actionSubtitle?: ReactNode
  identity: ReactNode
  subtitle?: ReactNode
}

// Match the reference's full-to-compact travel without reacting to every scroll frame in React.
const SHRINK_DISTANCE = 120
const COMPACT_SCALE = 0.625
const SUBTITLE_FADE_END = 0.5
const PADDING_REDUCTION = '(var(--spacing-2xl) - var(--spacing-sm))'
const SUBTITLE_HEIGHT = '(var(--spacing-lg) + var(--spacing-sm))'

export function MarketHeaderMotion({
  actions,
  actionSubtitle,
  identity,
  subtitle,
}: MarketHeaderMotionProps) {
  const { scrollY } = useScroll()
  const reducedMotion = useReducedMotion()
  const progress = useTransform(scrollY, offset =>
    reducedMotion ? Number(offset > 0) : Math.min(1, Math.max(0, offset / SHRINK_DISTANCE))
  )
  const scale = useTransform(progress, [0, 1], [1, COMPACT_SCALE])
  const subtitleOpacity = useTransform(progress, [0, SUBTITLE_FADE_END], [1, 0])
  const subtitleVisibility = useTransform(progress, value =>
    value >= SUBTITLE_FADE_END ? 'hidden' : 'visible'
  )
  const subtitleHeight = useTransform(progress, value => `calc(${SUBTITLE_HEIGHT} * ${1 - value})`)
  const paddingBlock = useTransform(
    progress,
    value => `calc(var(--spacing-sm) + ${PADDING_REDUCTION} * ${1 - value})`
  )
  // Keep the document's footprint constant so shrinking never moves the scroll anchor.
  const releasedSpace = useTransform(
    progress,
    value =>
      `calc((${PADDING_REDUCTION} * 2 + ${subtitle || actionSubtitle ? SUBTITLE_HEIGHT : '0px'}) * ${value})`
  )

  return (
    <>
      <motion.header
        aria-label="Market header"
        className="sticky top-app-nav z-20 bg-background"
        style={{ paddingBlock }}
      >
        <div className="relative flex min-h-touch items-start justify-between gap-sm">
          <div className="min-w-0">
            <motion.div className="origin-left" style={{ scale }}>
              {identity}
            </motion.div>
            {subtitle ? (
              <motion.div
                className="overflow-hidden"
                style={{
                  height: subtitleHeight,
                  opacity: subtitleOpacity,
                  visibility: subtitleVisibility,
                }}
              >
                <div className="mt-sm">{subtitle}</div>
              </motion.div>
            ) : null}
          </div>
          <div className="shrink-0">
            <div className="flex min-h-touch justify-end">
              <motion.div className="origin-right" style={{ scale }}>
                {actions}
              </motion.div>
            </div>
            {actionSubtitle ? (
              <motion.div
                className="overflow-hidden"
                style={{
                  height: subtitleHeight,
                  opacity: subtitleOpacity,
                  visibility: subtitleVisibility,
                }}
              >
                <div className="mt-sm">{actionSubtitle}</div>
              </motion.div>
            ) : null}
          </div>
        </div>
        <div
          aria-hidden="true"
          className="pointer-events-none absolute inset-x-0 bottom-0 h-0.5 bg-foreground"
        />
      </motion.header>
      <motion.div aria-hidden="true" style={{ height: releasedSpace }} />
    </>
  )
}
