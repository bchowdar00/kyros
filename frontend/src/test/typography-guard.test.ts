import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'

const SOURCE_ROOTS = [
  join(import.meta.dirname, '..'),
  join(import.meta.dirname, '../../packages/ui/src'),
]

function sourceFiles(directory: string): string[] {
  return readdirSync(directory, { recursive: true, withFileTypes: true })
    .filter(entry => entry.isFile() && /\.(ts|tsx|css)$/.test(entry.name))
    .map(entry => join(entry.parentPath, entry.name))
}

// Tabular figures have one home: `font-variant-numeric` on <body> in globals.css, so every
// number keeps its width as it updates. A `tabular-nums` utility on a component would be a
// second home for the same rule.
const TABULAR_HOME = join(import.meta.dirname, '../app/globals.css')

describe('typography guard', () => {
  it('sets tabular figures once, on the body', () => {
    expect(readFileSync(TABULAR_HOME, 'utf8')).toContain('font-variant-numeric: tabular-nums')

    const offenders = SOURCE_ROOTS.flatMap(sourceFiles)
      .filter(file => !file.includes('/test/') && file !== TABULAR_HOME)
      .filter(file => readFileSync(file, 'utf8').includes('tabular-nums'))

    expect(offenders).toEqual([])
  })
})
