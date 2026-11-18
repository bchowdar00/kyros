import { createTool } from '@mastra/core/tools'
import type { RustSignal, RustSignalCatalogue, RustSignalField } from '@tiny/domain'
import * as z from 'zod/mini'

import { venueOf } from '@/features/signals/signal-cards'
import { formatSignalValue, isNumericField } from '@/features/signals/signal-value-format'
import {
  exchangeClient,
  failed,
  resolveSeries,
  SeriesInputSchema,
  UNAVAILABLE,
} from '../shared/exchange'

const SEARCH_RESULTS_MAX = 25
const READ_REFS_MAX = 6
const SAMPLE_POINTS_MAX = 24
/** The bus retains twenty minutes; a read never asks for more. */
const WINDOW_MS_MAX = 20 * 60 * 1000
const WINDOW_MS_DEFAULT = 4 * 60 * 1000
const RETRIEVED = () => new Date().toISOString()

/**
 * Words a user says that the catalogue spells differently. Prediction venues call the
 * sides Up and Down and their fields "affirmative" and "negative"; a price is a quote.
 */
const ALIASES: Readonly<Record<string, readonly string[]>> = {
  yes: ['affirmative', 'up'],
  no: ['negative', 'down'],
  up: ['affirmative'],
  down: ['negative'],
  price: ['quote', 'spot', 'midpoint'],
  odds: ['probability'],
  flow: ['imbalance', 'aggressor'],
  volume: ['notional'],
}

/** The plain word behind a plural or a possessive: "prices" and "price's" both read "price". */
function stem(term: string): string {
  return term.replace(/'s$/, '').replace(/(?<=[a-z]{3})s$/, '')
}

/**
 * Everything a query word can match. The catalogue carries the server-authored copy: a
 * signal's name and description, and each field's label and description.
 */
function haystackOf(signal: RustSignal): string {
  return [
    signal.signal_id.replaceAll('.', ' ').replaceAll('_', ' '),
    signal.name,
    signal.description,
    venueOf(signal),
    ...signal.fields.map(
      field => `${field.name.replaceAll('_', ' ')} ${field.label} ${field.description ?? ''}`
    ),
  ]
    .join(' ')
    .toLowerCase()
}

/** Ready signals first, then live ones still warming up or degraded, then offline venues. */
function usability(signal: RustSignal): number {
  if (!signal.live) return 0
  return signal.quality.state === 'ready' ? 2 : 1
}

/**
 * How many of the query's words this signal answers to. A word matches by itself, by
 * its stem, or through an alias, so "Polymarket YES NO prices" reaches the venue's
 * quote instead of failing on words the catalogue never uses. Zero means no match.
 */
function matchScore(signal: RustSignal, terms: readonly string[]): number {
  const haystack = haystackOf(signal)
  let score = 0
  for (const term of terms) {
    const forms = [term, stem(term), ...(ALIASES[term] ?? []), ...(ALIASES[stem(term)] ?? [])]
    if (forms.some(form => form.length > 0 && haystack.includes(form))) score += 1
  }
  return score
}

/** One catalogue entry as the model reads it: readable names, headline fields, live values. */
function describe(signal: RustSignal, showAll: boolean) {
  return {
    signal_id: signal.signal_id,
    title: signal.name,
    venue: venueOf(signal),
    summary: signal.description,
    live: signal.live,
    quality: signal.quality.state,
    fields: signal.fields
      .filter(field => showAll || field.headline)
      .map(field => ({
        name: field.name,
        label: field.label,
        type: field.type,
        numeric: isNumericField(field),
        ...(field.description === undefined ? {} : { description: field.description }),
        ...(field.value === undefined ? {} : { value: formatSignalValue(signal, field) }),
      })),
  }
}

function findField(
  catalogue: RustSignalCatalogue,
  id: string,
  field: string
): { reason: string } | { signal: RustSignal; entry: RustSignalField } {
  const signal = catalogue.signals.find(entry => entry.signal_id === id)
  if (signal === undefined) return { reason: `${id} is not a signal of ${catalogue.series}.` }
  const entry = signal.fields.find(candidate => candidate.name === field)
  if (entry === undefined) return { reason: `${signal.name} has no field named ${field}.` }
  if (!isNumericField(entry)) return { reason: `${field} is a state, not a number.` }
  return { signal, entry }
}

export function createSignalTools(baseUrl: string | undefined) {
  const client = baseUrl === undefined ? undefined : exchangeClient(baseUrl)

  const searchSignals = createTool({
    id: 'search-signals',
    description:
      'Find signals that apply to a series by words in their name, venue, description or field labels: RSI, order flow, funding, Polymarket, and so on. Returns each signal’s id, its headline fields with names, types and current formatted values, and whether it is live. Use the exact signal_id and field name when reading or showing a signal.',
    inputSchema: z.object({
      series: SeriesInputSchema,
      query: z.optional(z.string().check(z.maxLength(120))),
      /** Include plumbing fields such as timestamps and echoed parameters. */
      include_all_fields: z.optional(z.boolean()),
    }),
    execute: async ({ series: requested, query = '', include_all_fields = false }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const catalogue = await client.getSignals(series, context?.abortSignal)
      if (!catalogue.ok) return failed('The signal catalogue could not be read.', series)
      const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
      // Best match first, then what can be shown right now, so the cap never trims a
      // live signal to keep an offline venue that happens to sit earlier in the catalogue.
      const found = catalogue.data.signals
        .map((signal, order) => ({ signal, order, score: matchScore(signal, terms) }))
        .filter(({ score }) => terms.length === 0 || score > 0)
        .toSorted(
          (a, b) =>
            b.score - a.score || usability(b.signal) - usability(a.signal) || a.order - b.order
        )
        .map(({ signal }) => describe(signal, include_all_fields))
        .filter(signal => signal.fields.length > 0)
      return {
        status: 'ok',
        source: 'Tiny exchange signals',
        retrievedAt: RETRIEVED(),
        series,
        total: found.length,
        signals: found.slice(0, SEARCH_RESULTS_MAX),
      }
    },
  })

  const readSignals = createTool({
    id: 'read-signals',
    description:
      'Read the recent values of up to six signal fields over a window: first, last, low and high plus a short evenly sampled trace per field, from the exchange’s retained history (at most 20 minutes). Use search_signals first for exact ids and field names.',
    inputSchema: z.object({
      series: SeriesInputSchema,
      refs: z
        .array(z.object({ signal_id: z.string(), field: z.string() }))
        .check(z.minLength(1), z.maxLength(READ_REFS_MAX)),
      window_ms: z.optional(z.int().check(z.minimum(10_000), z.maximum(WINDOW_MS_MAX))),
    }),
    execute: async ({ series: requested, refs, window_ms = WINDOW_MS_DEFAULT }, context) => {
      if (!client) return UNAVAILABLE
      const series = resolveSeries(requested, context)
      const catalogue = await client.getSignals(series, context?.abortSignal)
      if (!catalogue.ok) return failed('The signal catalogue could not be read.', series)
      const refused = refs
        .map(ref => findField(catalogue.data, ref.signal_id, ref.field))
        .flatMap(found => ('reason' in found ? [found.reason] : []))
      if (refused.length > 0) return failed(refused.join(' '), series)
      const history = await client.getSignalsHistory(
        {
          series,
          ids: [...new Set(refs.map(ref => ref.signal_id))],
          historyMs: window_ms,
          maxPoints: SAMPLE_POINTS_MAX,
        },
        context?.abortSignal
      )
      if (!history.ok) return failed('Signal history could not be read.', series)
      const fields = refs.map(ref => {
        const frames =
          history.data.signals.find(entry => entry.signal_id === ref.signal_id)?.frames ?? []
        const points = frames.flatMap(frame => {
          const raw = frame.values[ref.field]
          const value = typeof raw === 'boolean' ? Number.NaN : Number(raw)
          return Number.isFinite(value) ? [{ at_ms: frame.ts_ms, value }] : []
        })
        const values = points.map(point => point.value)
        const found = findField(catalogue.data, ref.signal_id, ref.field)
        const label = 'signal' in found ? found.entry.label : ref.field
        const formatted =
          'signal' in found && points.length > 0
            ? formatSignalValue(found.signal, { ...found.entry, value: String(values.at(-1)) })
            : undefined
        return {
          signal_id: ref.signal_id,
          field: ref.field,
          label,
          samples: points.length,
          ...(points.length === 0
            ? { note: 'No retained value in this window.' }
            : {
                first: values[0],
                last: values.at(-1),
                low: Math.min(...values),
                high: Math.max(...values),
                ...(formatted === undefined ? {} : { last_formatted: formatted }),
                points,
              }),
        }
      })
      return {
        status: 'ok',
        source: 'Tiny exchange signals',
        retrievedAt: RETRIEVED(),
        series,
        window_ms,
        fields,
      }
    },
  })

  return { searchSignals, readSignals }
}
