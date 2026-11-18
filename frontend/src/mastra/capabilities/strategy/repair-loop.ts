import type { AuthoringStrategy } from './authoring-schema'
import type { CompileOutcome, CompileSummary } from './compile-client'
import { projectToIr, type IrDocument } from './project-to-ir'

/** Repairs beyond this rarely converge; the user is better served by a question. */
export const MAX_REPAIRS = 3

/**
 * Compiler warnings that are structural bugs, not style: a block that re-fires on every
 * wake, a state-gated block an edge trigger can skip, a trigger no leg arms. They
 * compile, but they are exactly the strategies that fire forty times a round or never,
 * so the loop treats them as problems the writer must fix.
 */
const STRUCTURAL_WARNINGS = [
  'lint-machine-gun',
  'lint-missed-wake',
  'trigger-unarmed',
  'triggered-not-armed',
]

export function structuralWarnings(warnings: readonly string[]) {
  return warnings.filter(warning => STRUCTURAL_WARNINGS.some(code => warning.includes(`[${code}]`)))
}

export interface RepairLoopInput {
  authored: AuthoringStrategy
  compile: (strategy: IrDocument) => Promise<CompileOutcome>
  /** Asks the writer for a corrected document given what the compiler said. */
  repair: (authored: AuthoringStrategy, problems: string[]) => Promise<AuthoringStrategy>
  /**
   * Judges a document that compiled against the plan; each mismatch is a problem the
   * writer must fix, exactly like a diagnostic.
   */
  check?: (authored: AuthoringStrategy, summary: CompileSummary) => string[]
  maxRepairs?: number
}

export type RepairLoopResult =
  | {
      status: 'compiled'
      authored: AuthoringStrategy
      strategy: IrDocument
      summary: CompileSummary
      warnings: string[]
      /** Structural problems left after the last repair; empty when the document is clean. */
      mismatches: string[]
      attempts: number
    }
  | {
      status: 'failed'
      authored: AuthoringStrategy
      strategy: IrDocument
      problems: string[]
      attempts: number
    }

/**
 * Project, compile, and feed the diagnostics back to the writer until the document
 * compiles clean or the repair budget runs out. Projection warnings count as problems
 * too: they are structural mistakes the compiler would also refuse. So do the
 * compiler's structural warnings and the plan check's mismatches: a document that
 * compiles but has the wrong shape is repaired, not shipped.
 */
export async function compileWithRepairs(input: RepairLoopInput): Promise<RepairLoopResult> {
  const maxRepairs = input.maxRepairs ?? MAX_REPAIRS
  let authored = input.authored
  let attempts = 0
  for (;;) {
    attempts += 1
    const projection = projectToIr(authored)
    const problems = [...projection.warnings]
    if (problems.length === 0) {
      const outcome = await input.compile(projection.strategy)
      if (outcome.ok) {
        problems.push(...structuralWarnings(outcome.warnings))
        problems.push(...(input.check?.(authored, outcome.summary) ?? []))
        if (problems.length === 0 || attempts > maxRepairs)
          return {
            status: 'compiled',
            authored,
            strategy: projection.strategy,
            summary: outcome.summary,
            warnings: outcome.warnings,
            mismatches: problems,
            attempts,
          }
      } else
        problems.push(...(outcome.kind === 'diagnostics' ? outcome.rendered : [outcome.message]))
    }
    if (attempts > maxRepairs)
      return { status: 'failed', authored, strategy: projection.strategy, problems, attempts }
    authored = await input.repair(authored, problems)
  }
}
