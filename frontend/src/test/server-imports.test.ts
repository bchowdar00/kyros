import { readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

/**
 * The chat route and Teeny's tools run on the server. A `'use client'` module imported
 * there is not a module but a bundle of client references, and calling one throws at
 * request time with the route's generic "could not respond" text. This walks the server
 * entry points' value imports and names the first client module reached, so the break
 * shows up here rather than in a chat reply.
 */
const SRC = resolve(__dirname, '..')
const SERVER_ROOTS = ['mastra', 'app/api']
const IMPORT = /^\s*(?:import|export)\s+(?!type\s)[^'"]*?\sfrom\s+['"]([^'"]+)['"]/gm

function files(dir: string): string[] {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name)
    if (statSync(path).isDirectory()) return files(path)
    return /\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name) ? [path] : []
  })
}

function resolveImport(from: string, specifier: string): string | undefined {
  const base = specifier.startsWith('@/')
    ? join(SRC, specifier.slice(2))
    : specifier.startsWith('.')
      ? resolve(dirname(from), specifier)
      : undefined
  if (base === undefined) return undefined
  for (const candidate of [base, `${base}.ts`, `${base}.tsx`, join(base, 'index.ts')]) {
    try {
      if (statSync(candidate).isFile()) return candidate
    } catch {
      // Not this candidate; try the next extension.
    }
  }
  return undefined
}

function isClientModule(path: string): boolean {
  return /^\s*'use client'/.test(readFileSync(path, 'utf8'))
}

/** The first client module reachable from `entry` through value imports, with its path. */
function firstClientModule(entry: string, seen: Set<string>): string[] | undefined {
  const stack: string[][] = [[entry]]
  while (stack.length > 0) {
    const chain = stack.pop()
    const file = chain?.at(-1)
    if (chain === undefined || file === undefined || seen.has(file)) continue
    seen.add(file)
    if (isClientModule(file)) return chain
    const source = readFileSync(file, 'utf8')
    for (const match of source.matchAll(IMPORT)) {
      const specifier = match[1]
      const next = specifier === undefined ? undefined : resolveImport(file, specifier)
      if (next !== undefined) stack.push([...chain, next])
    }
  }
  return undefined
}

describe('server entry points', () => {
  it('never reach a client module through a value import', () => {
    const seen = new Set<string>()
    for (const root of SERVER_ROOTS) {
      for (const entry of files(join(SRC, root))) {
        const chain = firstClientModule(entry, seen)
        expect(
          chain?.map(path => path.slice(SRC.length + 1)).join(' -> '),
          `${entry.slice(SRC.length + 1)} imports a 'use client' module`
        ).toBeUndefined()
      }
    }
  })
})
