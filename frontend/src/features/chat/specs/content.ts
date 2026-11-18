import * as z from 'zod/mini'

import { TITLE_MAX } from './bound'

/**
 * Widgets whose data is the model's own words or arithmetic, carried inline. The frame
 * marks them as authored so a computed number is never mistaken for a live one.
 */
/** Untrusted text is cut before any regex runs (AGENTS.md §11). */
export const MARKDOWN_MAX = 2_000
export const CODE_MAX = 4_000
export const SOURCES_MAX = 10
export const DIFF_LINES_MAX = 80
export const TABLE_COLUMNS_MAX = 6
export const TABLE_ROWS_MAX = 50
export const STATS_MAX = 9
const SHORT_MAX = 200
const CELL_MAX = 80

const title = z.optional(z.string().check(z.minLength(1), z.maxLength(TITLE_MAX)))
const short = z.string().check(z.minLength(1), z.maxLength(SHORT_MAX))
const cell = z.string().check(z.maxLength(CELL_MAX))

export const MarkdownSpecSchema = z.object({
  kind: z.literal('markdown'),
  title,
  text: z.string().check(z.minLength(1), z.maxLength(MARKDOWN_MAX)),
})

export const CodeSpecSchema = z.object({
  kind: z.literal('code'),
  title,
  language: z.optional(z.string().check(z.maxLength(24))),
  code: z.string().check(z.minLength(1), z.maxLength(CODE_MAX)),
})

export const SourcesSpecSchema = z.object({
  kind: z.literal('sources'),
  title,
  sources: z
    .array(
      z.object({
        title: short,
        url: z.string().check(z.maxLength(SHORT_MAX * 4)),
        retrieved: z.optional(z.string().check(z.maxLength(40))),
        excerpt: z.optional(z.string().check(z.maxLength(SHORT_MAX))),
      })
    )
    .check(z.minLength(1), z.maxLength(SOURCES_MAX)),
})

export const DiffSpecSchema = z.object({
  kind: z.literal('diff'),
  title,
  from: short,
  to: short,
  lines: z
    .array(z.object({ kind: z.enum(['context', 'added', 'removed']), text: cell }))
    .check(z.minLength(1), z.maxLength(DIFF_LINES_MAX)),
})

/** An authored table: every cell is text the model wrote. */
/**
 * A row is its cells in column order. A record keyed by column id cannot be described to
 * the model's tool schema (free-form keys are dropped, and it sends `{}`), so the shape
 * is positional.
 */
export const TableSpecSchema = z.object({
  kind: z.literal('table'),
  title,
  columns: z
    .array(z.object({ id: z.string().check(z.minLength(1), z.maxLength(32)), label: cell }))
    .check(z.minLength(1), z.maxLength(TABLE_COLUMNS_MAX)),
  rows: z
    .array(
      z.array(z.union([cell, z.number()])).check(z.minLength(1), z.maxLength(TABLE_COLUMNS_MAX))
    )
    .check(z.minLength(1), z.maxLength(TABLE_ROWS_MAX)),
})

/** Authored figures with labels, for a comparison or a scenario the model worked out. */
export const StatGridSpecSchema = z.object({
  kind: z.literal('stat-grid'),
  title,
  stats: z
    .array(
      z.object({
        label: cell,
        value: cell,
        delta: z.optional(cell),
        direction: z.optional(z.enum(['up', 'down'])),
      })
    )
    .check(z.minLength(1), z.maxLength(STATS_MAX)),
})
