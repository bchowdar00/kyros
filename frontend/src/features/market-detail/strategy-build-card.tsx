'use client'

import { useEffect, useState } from 'react'
import { Button } from '@tiny/ui/components/button'
import { NanoCheckIcon } from '@tiny/ui/icons/nano-check-icon'
import { BUILD_STEP_LABELS, type BuildStatus, type BuildStep } from './strategy-build'
import { useStrategyBuild } from './use-strategy-build'

interface StrategyBuildCardProps {
  id: string
  /** Drops a finished build's readback or problems into the conversation. */
  onAsk: (prompt: string) => void
  /** Opens the deploy screen for a compiled build; absent until deploying is wired. */
  onDeploy?: ((build: BuildStatus) => void) | undefined
}

function formatElapsed(seconds: number) {
  const minutes = Math.floor(seconds / 60)
  return `${minutes}:${String(seconds % 60).padStart(2, '0')}`
}

function Spinner() {
  return (
    <span aria-hidden="true" className="relative flex size-md shrink-0 items-center justify-center">
      <span className="absolute inset-0 animate-ping rounded-full bg-foreground/25 motion-reduce:animate-none" />
      <span className="relative size-xs rounded-full bg-foreground" />
    </span>
  )
}

function StepRow({ step }: { step: BuildStep }) {
  const labels = BUILD_STEP_LABELS[step.id]
  const attempts =
    step.attempts !== undefined && step.attempts > 1 ? ` after ${step.attempts} attempts` : ''
  return (
    <li
      aria-current={step.status === 'running' ? 'step' : undefined}
      className={`flex min-h-lg items-center gap-sm text-label ${step.status === 'pending' ? 'text-muted-foreground/60' : step.status === 'running' ? 'text-foreground' : 'text-muted-foreground'}`}
    >
      {step.status === 'done' ? (
        <NanoCheckIcon className="size-md shrink-0 text-positive" />
      ) : step.status === 'running' ? (
        <Spinner />
      ) : (
        <span aria-hidden="true" className="inline-block size-md shrink-0 text-center">
          {step.status === 'failed' ? '×' : '·'}
        </span>
      )}
      <span>
        {step.status === 'done' ? `${labels.done}${attempts}` : labels.active}
        <span className="sr-only">, {step.status}</span>
      </span>
    </li>
  )
}

/** Everything the card no longer shows travels with the question, so Teeny can raise it. */
function askText(build: BuildStatus) {
  if (build.status === 'compiled' && build.result) {
    const { readback, assumptions, mismatches, warnings } = build.result
    const notes = [
      assumptions.length ? `It assumed: ${assumptions.join('; ')}.` : '',
      mismatches.length ? `It may not match what I asked: ${mismatches.join('; ')}.` : '',
      warnings.length ? `The compiler noted: ${warnings.join(' ')}` : '',
    ]
      .filter(Boolean)
      .join(' ')
    return `The strategy compiled. Readback: ${readback}${notes ? ` ${notes}` : ''} Anything I should change before deploying?`
  }
  const problems = build.result?.problems.join('\n') ?? build.error ?? 'unknown'
  return `The strategy could not be compiled. The compiler said:\n${problems}\nWhat should we change?`
}

export function StrategyBuildCard({ id, onAsk, onDeploy }: StrategyBuildCardProps) {
  const { build, unreachable } = useStrategyBuild(id)
  const running = build === undefined ? !unreachable : build.status === 'running'
  const [elapsed, setElapsed] = useState(0)
  useEffect(() => {
    if (!running) return undefined
    const timer = window.setInterval(() => setElapsed(seconds => seconds + 1), 1000)
    return () => window.clearInterval(timer)
  }, [running])
  const compiled = build?.status === 'compiled' && build.result ? build.result : undefined
  const headline = unreachable
    ? 'The build could not be reached.'
    : running
      ? 'Building your strategy'
      : compiled
        ? 'Strategy compiled'
        : build?.status === 'failed'
          ? 'The compiler could not accept the strategy'
          : 'The build stopped early'

  return (
    <section
      aria-label="Strategy build"
      aria-live="polite"
      className="rounded-md border border-action/25 bg-background/1 p-md"
    >
      <div className="flex items-center gap-sm">
        {running ? <Spinner /> : null}
        {compiled ? <NanoCheckIcon className="size-md shrink-0 text-positive" /> : null}
        <span className="min-w-0 flex-1 text-label font-medium text-foreground">{headline}</span>
        {running ? (
          <span className="font-mono text-caption text-muted-foreground">
            {formatElapsed(elapsed)}
          </span>
        ) : null}
      </div>
      {build && !compiled ? (
        <ol aria-label="Build steps" className="pt-sm">
          {build.steps.map(step => (
            <StepRow key={step.id} step={step} />
          ))}
        </ol>
      ) : null}
      {running ? (
        <p className="pt-sm text-caption text-muted-foreground">
          Compiling in the background. You can keep talking.
        </p>
      ) : null}
      {compiled && build ? (
        <div className="flex flex-col gap-sm pt-sm">
          <p className="text-body text-foreground">{compiled.readback}</p>
          <div className="flex gap-sm">
            <Button
              id={`deploy-${build.id}`}
              size="small"
              disabled={onDeploy === undefined}
              title={
                onDeploy === undefined ? 'Deploying compiled strategies lands next' : undefined
              }
              onClick={onDeploy ? () => onDeploy(build) : undefined}
            >
              Deploy
            </Button>
            <Button size="small" variant="secondary" onClick={() => onAsk(askText(build))}>
              Ask Teeny about this
            </Button>
          </div>
        </div>
      ) : null}
      {build && !compiled && !running ? (
        <div className="flex flex-col gap-sm pt-sm">
          <p role="alert" className="text-label text-foreground">
            {build.result?.problems.length
              ? `The compiler refused it after ${build.result.attempts} attempts.`
              : (build.error ?? 'Something went wrong.')}
          </p>
          <Button size="small" variant="secondary" onClick={() => onAsk(askText(build))}>
            Ask Teeny about this
          </Button>
        </div>
      ) : null}
    </section>
  )
}
