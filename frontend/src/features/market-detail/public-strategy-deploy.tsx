'use client'

import { Button } from '@tiny/ui/components/button'

import {
  allocationCents,
  allocationUnavailable,
  signalsUnavailable,
  type StrategyDeploy,
} from './exchange/deploy'

interface PublicStrategyDeployProps {
  templateId: string
  inverse: boolean
  /** The dollars-and-cents draft from the settings above. */
  allocation: string
  /** Signals the exchange cannot act on yet; deploying is refused while any is listed. */
  waitingOn?: readonly string[] | undefined
  deploy?: StrategyDeploy | undefined
}

/** The widget's footer: why Deploy is disabled, what the last deploy did, Remix and Deploy. */
export function PublicStrategyDeploy({
  templateId,
  inverse,
  allocation,
  waitingOn,
  deploy,
}: PublicStrategyDeployProps) {
  const reason =
    deploy === undefined
      ? 'Connect to an exchange to deploy.'
      : (signalsUnavailable(waitingOn) ??
        deploy.reason ??
        allocationUnavailable(allocation, deploy.availableBalanceCents))
  const allocated = allocationCents(allocation)

  return (
    <div className="flex flex-col gap-sm px-xl">
      {reason === undefined ? null : <p className="text-label text-muted-foreground">{reason}</p>}
      {deploy === undefined || deploy.notice === '' ? null : (
        <output className="text-label">{deploy.notice}</output>
      )}
      <div className="flex gap-sm">
        <Button
          variant="secondary"
          className="min-h-touch flex-1 border-highlight bg-transparent text-body"
        >
          Remix
        </Button>
        <Button
          disabled={reason !== undefined || allocated === undefined}
          onClick={() => {
            if (deploy !== undefined && allocated !== undefined)
              void deploy.submit({ templateId, inverse, allocationCents: allocated })
          }}
          className="min-h-touch flex-1 text-body disabled:opacity-100"
        >
          Deploy Strategy
        </Button>
      </div>
    </div>
  )
}
