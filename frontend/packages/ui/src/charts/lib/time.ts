import type { TimeRange } from './types'

export const SECOND = 1_000
export const MINUTE = 60 * SECOND
export const HOUR = 60 * MINUTE
export const DAY = 24 * HOUR

/** Candidate spacings for time axis ticks, smallest first. */
export const TIME_TICK_STEPS: readonly number[] = [
  SECOND,
  2 * SECOND,
  5 * SECOND,
  10 * SECOND,
  15 * SECOND,
  30 * SECOND,
  MINUTE,
  2 * MINUTE,
  3 * MINUTE,
  5 * MINUTE,
  10 * MINUTE,
  15 * MINUTE,
  30 * MINUTE,
  HOUR,
  2 * HOUR,
  3 * HOUR,
  6 * HOUR,
  12 * HOUR,
  DAY,
  2 * DAY,
  7 * DAY,
  14 * DAY,
  30 * DAY,
  90 * DAY,
  365 * DAY,
]

export const DEFAULT_LOCALE = 'en-US'

export type TimeLabelDetail = 'day' | 'minutes' | 'month' | 'seconds'

export interface TimeFormatOptions {
  locale?: string | undefined
  timeZone?: string | undefined
}

/** Smallest step that produces at most `maxTicks` ticks across `spanMs`. */
export function chooseTimeStep(spanMs: number, maxTicks: number): number {
  const safeMaxTicks = Math.max(1, maxTicks)

  for (const step of TIME_TICK_STEPS) {
    if (spanMs / step <= safeMaxTicks) {
      return step
    }
  }

  return TIME_TICK_STEPS.at(-1) ?? DAY
}

const offsetFormatters = new Map<string, Intl.DateTimeFormat>()

function getOffsetFormatter(timeZone: string) {
  let formatter = offsetFormatters.get(timeZone)

  if (!formatter) {
    formatter = new Intl.DateTimeFormat(DEFAULT_LOCALE, {
      day: 'numeric',
      hour: 'numeric',
      hourCycle: 'h23',
      minute: 'numeric',
      month: 'numeric',
      second: 'numeric',
      timeZone,
      year: 'numeric',
    })
    offsetFormatters.set(timeZone, formatter)
  }

  return formatter
}

/** Milliseconds to add to a UTC epoch to obtain wall-clock time in `timeZone`. */
export function getTimeZoneOffset(time: number, timeZone?: string): number {
  if (!timeZone) {
    return -new Date(time).getTimezoneOffset() * MINUTE
  }

  const parts: Record<string, number> = {}

  for (const part of getOffsetFormatter(timeZone).formatToParts(new Date(time))) {
    if (part.type !== 'literal') {
      parts[part.type] = Number(part.value)
    }
  }

  const wallClock = Date.UTC(
    parts['year'] ?? 1970,
    (parts['month'] ?? 1) - 1,
    parts['day'] ?? 1,
    parts['hour'] ?? 0,
    parts['minute'] ?? 0,
    parts['second'] ?? 0
  )

  return wallClock - Math.floor(time / SECOND) * SECOND
}

export interface TimeTicksOptions extends TimeFormatOptions {
  maxTicks: number
}

export interface TimeTicks {
  detail: TimeLabelDetail
  step: number
  values: readonly number[]
}

/** Among steps that fit the budget, prefers one whose ticks land on the window's start. */
export function chooseAlignedTimeStep(range: TimeRange, maxTicks: number, offset: number): number {
  const span = range.end - range.start
  const smallest = chooseTimeStep(span, maxTicks)
  const wallStart = range.start + offset

  for (const step of TIME_TICK_STEPS) {
    if (step < smallest) {
      continue
    }

    if (step > smallest * 4 || span / step < 2) {
      break
    }

    if (wallStart % step === 0) {
      return step
    }
  }

  return smallest
}

/** Tick times aligned to wall-clock boundaries (for example every 15 minutes on the quarter hour). */
export function timeTicks(range: TimeRange, { maxTicks, timeZone }: TimeTicksOptions): TimeTicks {
  const offset = getTimeZoneOffset(range.start, timeZone)
  const step = chooseAlignedTimeStep(range, maxTicks, offset)
  const first = Math.ceil((range.start + offset) / step) * step - offset
  const values: number[] = []

  for (let time = first; time <= range.end; time += step) {
    values.push(time)
  }

  return { detail: getTimeLabelDetail(step), step, values }
}

export function getTimeLabelDetail(stepMs: number): TimeLabelDetail {
  if (stepMs < MINUTE) {
    return 'seconds'
  }

  if (stepMs < DAY) {
    return 'minutes'
  }

  if (stepMs < 30 * DAY) {
    return 'day'
  }

  return 'month'
}

const dateFormatters = new Map<string, Intl.DateTimeFormat>()

function getDateFormatter(options: Intl.DateTimeFormatOptions, locale = DEFAULT_LOCALE) {
  const key = `${locale}|${JSON.stringify(options)}`
  let formatter = dateFormatters.get(key)

  if (!formatter) {
    formatter = new Intl.DateTimeFormat(locale, options)
    dateFormatters.set(key, formatter)
  }

  return formatter
}

/** Intl inserts narrow no-break spaces before day periods; normalise them for consistent rendering. */
function normalizeSpaces(value: string) {
  return value.replaceAll(' ', ' ').replaceAll(' ', ' ')
}

const detailFormats: Record<TimeLabelDetail, Intl.DateTimeFormatOptions> = {
  day: { day: 'numeric', month: 'short' },
  minutes: { hour: 'numeric', minute: '2-digit' },
  month: { month: 'short', year: 'numeric' },
  seconds: { hour: 'numeric', minute: '2-digit', second: '2-digit' },
}

export function formatTimeTick(
  time: number,
  detail: TimeLabelDetail,
  { locale, timeZone }: TimeFormatOptions = {}
): string {
  const options: Intl.DateTimeFormatOptions = { ...detailFormats[detail] }

  if (timeZone) {
    options.timeZone = timeZone
  }

  return normalizeSpaces(getDateFormatter(options, locale).format(time))
}

export interface TooltipTimeOptions extends TimeFormatOptions {
  showDate?: boolean
  showSeconds?: boolean
}

/** Full timestamp for tooltips, for example `May 22, 8:27:14 PM`. */
export function formatTooltipTime(
  time: number,
  { locale, showDate = true, showSeconds = true, timeZone }: TooltipTimeOptions = {}
): string {
  const options: Intl.DateTimeFormatOptions = {
    hour: 'numeric',
    minute: '2-digit',
  }

  if (showSeconds) {
    options.second = '2-digit'
  }

  if (showDate) {
    options.month = 'short'
    options.day = 'numeric'
  }

  if (timeZone) {
    options.timeZone = timeZone
  }

  return normalizeSpaces(getDateFormatter(options, locale).format(time))
}

/** Short time-zone name such as `PT` or `ET`, falling back to the offset when unavailable. */
export function formatTimeZoneAbbreviation(time: number, timeZone?: string, locale?: string) {
  const options: Intl.DateTimeFormatOptions = { timeZoneName: 'short' }

  if (timeZone) {
    options.timeZone = timeZone
  }

  const part = getDateFormatter(options, locale)
    .formatToParts(time)
    .find(item => item.type === 'timeZoneName')

  return part?.value.replace(/^([A-Z])[SD]T$/, '$1T') ?? ''
}

/** `May 22, 8:25 PM - 8:30 PM PT` */
export function formatTimeRange(range: TimeRange, timeZone: string, locale?: string) {
  const start = formatTooltipTime(range.start, { locale, showSeconds: false, timeZone })
  const end = formatTooltipTime(range.end, {
    locale,
    showDate: false,
    showSeconds: false,
    timeZone,
  })

  return `${start} - ${end} ${formatTimeZoneAbbreviation(range.end, timeZone, locale)}`
}
