import type { KnipConfig } from 'knip'

// Dead code, unused exports, and unused or unlisted dependencies fail `pnpm check`.
// Every ignore below carries the reason it exists. Remove the entry when the reason is gone.
const config: KnipConfig = {
  // Tailwind and the token stylesheets are pulled in through CSS `@import`, which knip does
  // not read on its own. Surface those as imports so the dependencies count as used.
  compilers: {
    css: (text: string) =>
      [...text.matchAll(/@import\s+["']([^"']+)["']/g)]
        .map(match => `import '${match[1]}'`)
        .join('\n'),
  },
  // A type exported for readers but only consumed in its own file is documentation, not debt.
  ignoreExportsUsedInFile: true,
  workspaces: {
    '.': {
      entry: ['e2e/**/*.e2e.ts', 'e2e/**/*.dev.ts', 'src/mastra/index.ts'],
      project: ['src/**/*.{ts,tsx,css}', 'e2e/**/*.ts', '*.ts'],
      ignoreDependencies: [
        // Development element picker loaded by URL through Next Script in app/layout.tsx.
        'react-grab',
        // Chosen and pinned ahead of the data layer (AGENTS.md §7). Each is imported by its
        // first consumer so no provider ships to the client before something uses it.
        'nuqs',
        // Chosen utility library (AGENTS.md §1). Imported per function when first needed.
        'es-toolkit',
      ],
    },
    'packages/ui': {
      entry: [
        'src/components/*.tsx',
        'src/icons/*.tsx',
        'src/lib/*.ts',
        'src/charts/*.tsx',
        'src/charts/lib/*.ts',
        'scripts/*.mjs',
      ],
      project: ['src/**/*.{ts,tsx,css}', 'scripts/**/*.mjs'],
    },
    'packages/money': {},
    'packages/domain': {},
    'packages/api': {},
    'packages/testing': {},
    'packages/config/postcss-config': {},
    'packages/config/typescript-config': {},
  },
}

export default config
