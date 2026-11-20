'use client'

import { createMoneyFormatter } from '@kyros/money'
import { Avatar } from '@kyros/ui/components/avatar'
import { AvatarGroup } from '@kyros/ui/components/avatar-group'
import { Button } from '@kyros/ui/components/button'
import { IconButton } from '@kyros/ui/components/icon-button'
import { ShareIcon } from '@kyros/ui/icons/share-icon'
import Image from 'next/image'
import { useState } from 'react'

import type { StrategyDeploy } from './exchange/deploy'
import { PublicStrategyDeploy } from './public-strategy-deploy'
import { cardSideTitle } from './strategy-card-names'
import { PublicStrategySettings } from './public-strategy-settings'
import { StrategyLogicTabs } from './strategy-logic-tabs'
import type { PublicStrategy } from './types'
import { WidgetBackButton } from './widget-back-button'

interface PublicStrategyWidgetProps {
  strategy: PublicStrategy
  inverse: boolean
  scopeLabel: string
  /** How Deploy Strategy deploys; absent without a live exchange, which disables it. */
  deploy?: StrategyDeploy | undefined
  onClose: () => void
  /** A short line about the data shown, such as a failed refresh. */
  status?: string | undefined
}

const money = createMoneyFormatter('en-US')
const DEFAULT_ALLOCATION = '100.00'

export function PublicStrategyWidget({
  strategy,
  inverse,
  scopeLabel,
  deploy,
  onClose,
  status,
}: PublicStrategyWidgetProps) {
  const [allocation, setAllocation] = useState(DEFAULT_ALLOCATION)
  return (
    <dialog
      open
      aria-label="Public strategy"
      className="relative m-0 w-full max-w-none max-h-widget-viewport overflow-y-auto overscroll-contain border-0 bg-background/50 p-0 pb-xl text-foreground"
    >
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-0 overflow-clip bg-surface"
      >
        {strategy.creator.avatar ? (
          <div className="absolute left-1/2 top-0 size-public-strategy-glow -translate-x-1/2 -translate-y-1/2 rounded-pill opacity-15 blur-public-strategy-glow">
            <Image
              alt=""
              src={strategy.creator.avatar.src}
              fill
              sizes="554px"
              className="rounded-pill object-cover"
            />
          </div>
        ) : null}
      </div>
      <div className="relative">
        <div className="flex flex-col gap-xl p-xl">
          <div className="flex items-center justify-between">
            <WidgetBackButton onClose={onClose} />
            <div className="flex items-center gap-sm">
              <span className="text-label text-muted-foreground">Public Strategy</span>
              <IconButton
                aria-label="Share public strategy"
                variant="ghost"
                size="medium"
                className="relative text-foreground after:absolute after:-inset-copy after:content-['']"
              >
                <ShareIcon />
              </IconButton>
            </div>
          </div>
          <div className="flex items-center gap-md">
            <Avatar
              alt={strategy.creator.avatar?.alt ?? `${strategy.creator.handle} avatar`}
              fallback={strategy.creator.handle.slice(1, 3).toUpperCase()}
              framed
              size="large"
              {...(strategy.creator.avatar === undefined
                ? {}
                : { src: strategy.creator.avatar.src })}
            />
            <div className="min-w-0">
              <h2 className="text-heading-md font-regular">
                {cardSideTitle(strategy.title, inverse)}
              </h2>
              <p className="mt-copy text-label font-medium text-muted-foreground">
                By {strategy.creator.handle}
              </p>
            </div>
          </div>
        </div>
        <div className="mx-xl flex flex-wrap items-center gap-md border-y border-highlight py-xl">
          {strategy.runners ? (
            <AvatarGroup aria-hidden="true">
              {strategy.runners.map(avatar => (
                <Image
                  key={avatar.src}
                  alt=""
                  src={avatar.src}
                  width={20}
                  height={20}
                  className="size-avatar-sm rounded-pill"
                />
              ))}
            </AvatarGroup>
          ) : null}
          <p className="text-label text-muted-foreground">
            <span className="font-mono text-body text-foreground">
              {money.count(strategy.runningCount)}
            </span>{' '}
            running
          </p>
          <p className="text-label text-muted-foreground">
            <span className="font-mono text-body text-foreground">
              {money.usd(strategy.deployedCents, { compact: true, maximumFractionDigits: 2 })}
            </span>{' '}
            deployed
          </p>
          {status === undefined ? null : (
            <output className="basis-full text-label text-muted-foreground">{status}</output>
          )}
        </div>
        <div className="p-xl pb-0">
          <StrategyLogicTabs logic={inverse ? strategy.inverseLogic : strategy.logic} />
        </div>
        <div className="px-xl pt-xl pb-xl">
          <PublicStrategySettings
            scopeLabel={scopeLabel}
            allocation={allocation}
            onAllocationChange={setAllocation}
          />
          <Button
            variant="ghost"
            className="mt-sm min-h-touch w-full text-label font-medium text-muted-foreground"
          >
            View Advanced Settings
          </Button>
        </div>
        <PublicStrategyDeploy
          templateId={strategy.templateId}
          title={strategy.title}
          inverse={inverse}
          allocation={allocation}
          waitingOn={strategy.waitingOn}
          deploy={deploy}
        />
      </div>
    </dialog>
  )
}
