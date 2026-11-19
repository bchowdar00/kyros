'use client'

import { useState } from 'react'
import * as z from 'zod/mini'
import { Button } from '@kyros/ui/components/button'
import { StrategyBriefSchema, type StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { describeMoney } from '@/mastra/capabilities/strategy/envelope'
import type { BuildStatus } from './strategy-build'
import { StrategyBuildCard } from './strategy-build-card'
import { startStrategyBuild } from './use-strategy-build'

/** What the proposeCompilation tool answers; anything else is not a proposal. */
export const CompileProposalSchema = z.object({
  status: z.literal('proposed'),
  brief: StrategyBriefSchema,
  summary: z.string(),
})

interface CompileProposalCardProps {
  brief: StrategyBrief
  onAsk: (prompt: string) => void
  onKeepTalking: () => void
  /** Opens the deploy screen for a compiled build. */
  onDeploy?: ((build: BuildStatus, brief: StrategyBrief) => void) | undefined
}

/** The line shown when a build could not be started for an unexpected reason. */
const COULD_NOT_START = 'The build could not be started. Try again in a moment.'

type Phase =
  | { kind: 'proposed' }
  | { kind: 'starting' }
  /** A refusal to show the user: the route's resting / not-switched-on line, or the fallback. */
  | { kind: 'refused'; message: string }
  | { kind: 'building'; id: string }

/**
 * The confirmation gate: the plan in the user's terms, and nothing compiles until they
 * press Compile. Keep talking hides the card; Kyros proposes again when the brief changes.
 */
export function CompileProposalCard({
  brief,
  onAsk,
  onKeepTalking,
  onDeploy,
}: CompileProposalCardProps) {
  const [phase, setPhase] = useState<Phase>({ kind: 'proposed' })
  if (phase.kind === 'building')
    return (
      <StrategyBuildCard
        id={phase.id}
        onAsk={onAsk}
        onDeploy={onDeploy ? build => onDeploy(build, brief) : undefined}
      />
    )

  async function compile() {
    setPhase({ kind: 'starting' })
    const outcome = await startStrategyBuild(brief)
    switch (outcome.kind) {
      case 'started':
        setPhase({ kind: 'building', id: outcome.id })
        return
      case 'notice':
        setPhase({ kind: 'refused', message: outcome.message })
        return
      default:
        setPhase({ kind: 'refused', message: COULD_NOT_START })
    }
  }

  return (
    <section
      aria-label="Strategy proposal"
      className="rounded-md border border-action/25 bg-background/1 p-md"
    >
      <p className="text-body text-foreground">{brief.summary}</p>
      <p className="pt-xs text-label text-muted-foreground">{describeMoney(brief)}</p>
      {phase.kind === 'refused' ? (
        <p role="alert" className="pt-sm text-label text-foreground">
          {phase.message}
        </p>
      ) : null}
      <div className="flex gap-sm pt-sm">
        <Button size="small" disabled={phase.kind === 'starting'} onClick={() => void compile()}>
          {phase.kind === 'starting' ? 'Starting…' : 'Compile'}
        </Button>
        <Button size="small" variant="ghost" onClick={onKeepTalking}>
          Keep talking
        </Button>
      </div>
    </section>
  )
}
