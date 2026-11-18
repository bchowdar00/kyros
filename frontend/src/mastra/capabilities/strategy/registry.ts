import { createRustExchangeClient, type ApiResult } from '@tiny/api'
import type { RustSignal, RustSignalCatalogue } from '@tiny/domain'

/**
 * The venue registry as the writer sees it. The exchange's signal catalogue
 * (`GET /api/signals?series=`) already scopes the registry to one series and says
 * which signals are being published, so the scout works from a compact index of it
 * and the writer gets the full definitions of only the signals it chose.
 */

const REQUEST_TIMEOUT_MS = 5_000
/** Descriptions run to a paragraph; the index keeps the first sentence. */
const SUMMARY_MAX_CHARS = 160

export function instrumentId(series: string) {
  return `${series}.current`
}

export function createVenueRegistry(apiUrl: string) {
  const client = createRustExchangeClient({
    baseUrl: apiUrl,
    timeoutMs: REQUEST_TIMEOUT_MS,
    retry: { attempts: 1 },
    fetch: (input, init) => fetch(input, { ...init, cache: 'no-store' }),
  })
  return {
    catalogue(series: string, signal?: AbortSignal): Promise<ApiResult<RustSignalCatalogue>> {
      return client.getSignals(series, signal)
    },
    async series(series: string, signal?: AbortSignal) {
      const list = await client.listSeries(signal)
      return list.ok ? list.data.find(entry => entry.series === series) : undefined
    },
  }
}

export interface SignalIndexEntry {
  id: string
  name: string
  summary: string
  fields: string
}

function firstSentence(text: string) {
  const sentence = text.split(/(?<=\.)\s/)[0] ?? text
  return sentence.length > SUMMARY_MAX_CHARS
    ? `${sentence.slice(0, SUMMARY_MAX_CHARS - 1).trimEnd()}…`
    : sentence
}

function fieldSummary(signal: RustSignal) {
  return signal.fields
    .map(field =>
      field.type === 'enum' && field.variants
        ? `${field.name}:enum(${field.variants.join('|')})`
        : `${field.name}:${field.type}`
    )
    .join(', ')
}

/** Every signal a strategy on this series could subscribe to and actually hear from. */
export function signalIndex(catalogue: RustSignalCatalogue): SignalIndexEntry[] {
  return catalogue.signals
    .filter(signal => signal.live)
    .map(signal => ({
      id: signal.signal_id,
      name: signal.name,
      summary: firstSentence(signal.description),
      fields: fieldSummary(signal),
    }))
}

export function renderSignalIndex(entries: readonly SignalIndexEntry[]) {
  return entries
    .map(entry => `- ${entry.id} — ${entry.name}. Fields: ${entry.fields}. ${entry.summary}`)
    .join('\n')
}

/** The full registry entries for the chosen ids, in catalogue order; unknown ids are reported. */
export function selectSignals(catalogue: RustSignalCatalogue, ids: readonly string[]) {
  const wanted = new Set(ids)
  const chosen = catalogue.signals.filter(signal => wanted.has(signal.signal_id))
  const found = new Set(chosen.map(signal => signal.signal_id))
  return { chosen, unknown: ids.filter(id => !found.has(id)) }
}

export function renderSignalDefinitions(signals: readonly RustSignal[]) {
  return signals
    .map(signal => {
      const fields = signal.fields
        .map(field =>
          field.type === 'enum' && field.variants
            ? `  - ${field.name} (enum: ${field.variants.join(', ')}) — ${field.label}`
            : `  - ${field.name} (${field.type}) — ${field.label}`
        )
        .join('\n')
      return `### ${signal.signal_id}\n${signal.name}. ${signal.description}\nFields:\n${fields}`
    })
    .join('\n\n')
}
