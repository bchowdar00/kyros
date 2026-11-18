export interface PathPoint {
  x: number
  y: number
}

export type LineCurve = 'linear' | 'monotone' | 'step'

function round(value: number) {
  return Math.round(value * 100) / 100
}

/**
 * Fritsch-Carlson tangents: the cubic through the samples cannot overshoot a
 * local extreme, so a smooth line never invents a high the data does not have.
 */
function monotoneTangents(points: readonly PathPoint[]): number[] {
  const slopes: number[] = []

  for (let index = 0; index < points.length - 1; index += 1) {
    const from = points[index]
    const to = points[index + 1]
    const run = from && to ? to.x - from.x : 0

    slopes.push(from && to && run !== 0 ? (to.y - from.y) / run : 0)
  }

  return points.map((_, index) => {
    const before = slopes[index - 1]
    const after = slopes[index]

    if (before === undefined) {
      return after ?? 0
    }

    if (after === undefined) {
      return before
    }

    // A turn in the data pins the tangent flat; that is what kills the wobble.
    return before * after <= 0 ? 0 : (2 * before * after) / (before + after)
  })
}

function buildMonotonePath(points: readonly PathPoint[]): string {
  const first = points[0]

  if (!first) {
    return ''
  }

  const tangents = monotoneTangents(points)
  const commands: string[] = [`M${round(first.x)} ${round(first.y)}`]

  for (let index = 1; index < points.length; index += 1) {
    const from = points[index - 1]
    const to = points[index]
    const fromTangent = tangents[index - 1] ?? 0
    const toTangent = tangents[index] ?? 0

    if (!from || !to) {
      continue
    }

    const third = (to.x - from.x) / 3

    commands.push(
      `C${round(from.x + third)} ${round(from.y + fromTangent * third)},${round(to.x - third)} ${round(
        to.y - toTangent * third
      )},${round(to.x)} ${round(to.y)}`
    )
  }

  return commands.join('')
}

/** Builds an SVG path for a polyline, a smooth monotone curve or a step-after series. */
export function buildLinePath(points: readonly PathPoint[], curve: LineCurve = 'linear'): string {
  const first = points[0]

  if (!first) {
    return ''
  }

  if (curve === 'monotone') {
    return buildMonotonePath(points)
  }

  const commands: string[] = [`M${round(first.x)} ${round(first.y)}`]

  for (let index = 1; index < points.length; index += 1) {
    const point = points[index]

    if (!point) {
      continue
    }

    if (curve === 'step') {
      commands.push(`H${round(point.x)}V${round(point.y)}`)
    } else {
      commands.push(`L${round(point.x)} ${round(point.y)}`)
    }
  }

  return commands.join('')
}

/** Closes a line path down to `baselineY` so it can be filled as an area. */
export function buildAreaPath(
  points: readonly PathPoint[],
  baselineY: number,
  curve: LineCurve = 'linear'
): string {
  const first = points[0]
  const last = points.at(-1)

  if (!first || !last) {
    return ''
  }

  const line = buildLinePath(points, curve)

  return `${line}L${round(last.x)} ${round(baselineY)}L${round(first.x)} ${round(baselineY)}Z`
}
