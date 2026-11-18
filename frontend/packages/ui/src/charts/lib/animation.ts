export function easeOutCubic(progress: number) {
  return 1 - (1 - progress) ** 3
}

export function easeOutExpo(progress: number) {
  return progress >= 1 ? 1 : 1 - 2 ** (-10 * progress)
}

/** Scripted motion runs only with animation frames available and no reduced-motion preference. */
export const canAnimate = () =>
  typeof window !== 'undefined' &&
  typeof window.requestAnimationFrame === 'function' &&
  !(
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches
  )

/**
 * Drives `onFrame` with an eased 0–1 progress for `duration` ms and returns a
 * cancel function. Falls back to a single completed frame without rAF.
 */
export function runAnimation(
  duration: number,
  onFrame: (progress: number) => void,
  easing: (progress: number) => number = easeOutCubic
) {
  if (!canAnimate() || duration <= 0) {
    onFrame(1)

    return () => {}
  }

  let frame = 0
  let startedAt: number | null = null

  const tick = (now: number) => {
    startedAt ??= now
    const progress = Math.min(1, Math.max(0, (now - startedAt) / duration))
    onFrame(easing(progress))

    if (progress < 1) {
      frame = window.requestAnimationFrame(tick)
    }
  }

  frame = window.requestAnimationFrame(tick)

  return () => {
    window.cancelAnimationFrame(frame)
  }
}

/** Longest frame delta fed to a simulation; keeps a background tab from teleporting. */
const MAX_FRAME_MS = 64

/**
 * Calls `onFrame(dtMs, now)` every animation frame until it returns `false`.
 * Returns a cancel function. Without rAF a single frame runs immediately.
 */
export function startFrameLoop(onFrame: (dtMs: number, now: number) => boolean) {
  if (!canAnimate()) {
    onFrame(MAX_FRAME_MS, 0)

    return () => {}
  }

  let frame = 0
  let last: number | null = null

  const tick = (now: number) => {
    const dt = last === null ? 16 : Math.min(MAX_FRAME_MS, Math.max(1, now - last))
    last = now

    if (onFrame(dt, now)) {
      frame = window.requestAnimationFrame(tick)
    }
  }

  frame = window.requestAnimationFrame(tick)

  return () => {
    window.cancelAnimationFrame(frame)
  }
}

export interface SpringState {
  value: number
  velocity: number
}

/**
 * One step of a critically damped spring towards `target` (Unity's SmoothDamp).
 * Velocity carries between steps, so a target that moves mid-flight bends the
 * motion instead of restarting it. `smoothTime` is roughly the time to cover
 * most of the distance, in the same unit as `dt`.
 */
export function smoothDamp(
  state: SpringState,
  target: number,
  smoothTime: number,
  dt: number
): SpringState {
  const omega = 2 / Math.max(smoothTime, 1e-6)
  const x = omega * dt
  const decay = 1 / (1 + x + 0.48 * x * x + 0.235 * x * x * x)
  const change = state.value - target
  const temp = (state.velocity + omega * change) * dt
  const velocity = (state.velocity - omega * temp) * decay
  let value = target + (change + temp) * decay

  // Never overshoot the target.
  if (target - state.value > 0 === value > target) {
    value = target

    return { value, velocity: 0 }
  }

  return { value, velocity }
}
