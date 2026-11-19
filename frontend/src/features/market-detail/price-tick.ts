import type { ChartPoint } from '@kyros/ui/charts/lib/types'
import * as z from 'zod/mini'

export interface PriceTick {
  expo: number
  mantissa: string
  publishTimeUs: number
  symbol: string
  v: number
}

const PriceTickSchema = z.object({
  expo: z.int(),
  mantissa: z.string().check(z.regex(/^-?\d+$/)),
  publishTimeUs: z.pipe(
    z.union([z.number(), z.string().check(z.regex(/^-?\d+$/))]),
    z.transform(value => (typeof value === 'number' ? value : Number(value)))
  ),
  symbol: z.string(),
  v: z.literal(1),
})

export function parsePriceTick(value: unknown, expectedSymbol: string): PriceTick | undefined {
  const parsed = PriceTickSchema.safeParse(value)

  if (!parsed.success || parsed.data.symbol !== expectedSymbol) {
    return undefined
  }

  return parsed.data
}

export function decimalToNumber(mantissa: string, exponent: number): number | undefined {
  const value = Number(mantissa) * 10 ** exponent

  if (!Number.isFinite(value)) {
    return undefined
  }

  return value
}

export function tickToChartPoint(tick: PriceTick): ChartPoint | undefined {
  const value = decimalToNumber(tick.mantissa, tick.expo)

  if (value === undefined) {
    return undefined
  }

  return { time: Math.floor(tick.publishTimeUs / 1000), value }
}
