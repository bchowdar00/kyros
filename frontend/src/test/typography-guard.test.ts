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

// The Tiny Roobert proportional cut inverts `tnum` into narrow proportional figures, so
// `tabular-nums` CAUSES the width jitter it is meant to prevent (AGENTS.md §5). Updating
// numbers render in the mono cut (`font-mono`) instead.
describe('typography guard', () => {
  it('never reintroduces tabular-nums', () => {
    const offenders = SOURCE_ROOTS.flatMap(sourceFiles)
      .filter(file => !file.includes('/test/'))
      .filter(file => readFileSync(file, 'utf8').includes('tabular-nums'))

    expect(offenders).toEqual([])
  })
})
