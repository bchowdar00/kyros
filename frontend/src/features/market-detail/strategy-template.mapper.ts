import type { RustStrategyTemplate, RustStrategyTemplateCatalogue } from '@kyros/domain'
import { cents } from '@kyros/money'

import type { ImageAsset, PublicStrategy } from './types'

/** The exchange names portraits by file; the app serves them from here. */
const AVATAR_ROOT = '/assets/market/avatars/'
const TENTHS_PER_CENT = 10

function avatar(file: string, alt: string): ImageAsset {
  return { alt, src: `${AVATAR_ROOT}${file}` }
}

/** A card whose every signal is ready, so a deployment can fire. */
export function deployable(strategy: PublicStrategy): boolean {
  return strategy.waitingOn === undefined
}

/** One catalogue entry as the strategy card and widget render it. */
export function toPublicStrategy(template: RustStrategyTemplate): PublicStrategy {
  const portrait = template.creator.avatar
  const runners = template.runners.map(file => avatar(file, ''))
  // An exchange that predates readiness reports nothing, which reads as ready.
  const waitingOn = (template.waiting_on ?? []).map(signal => signal.name)

  return {
    id: template.id,
    templateId: template.id,
    title: template.title,
    logic: template.primary.logic,
    inverseLogic: template.inverse.logic,
    creator: {
      handle: template.creator.handle,
      username: template.creator.username,
      ...(portrait === null
        ? {}
        : { avatar: avatar(portrait, `${template.creator.handle} avatar`) }),
    },
    runningCount: template.running,
    deployedCents: cents(Math.round(template.deployed_tenths / TENTHS_PER_CENT)),
    ...(runners.length === 0 ? {} : { runners }),
    ...(waitingOn.length === 0 ? {} : { waitingOn }),
  }
}

/**
 * The cards the column shows: catalogue order, only the templates that can deploy. A card
 * waiting on a signal is hidden rather than offered, since running it would park the
 * allocation in an idle sub-account with nothing on the wire saying why.
 */
export function toPublicStrategies(
  catalogue: RustStrategyTemplateCatalogue
): readonly PublicStrategy[] {
  return catalogue.templates.map(toPublicStrategy).filter(deployable)
}
