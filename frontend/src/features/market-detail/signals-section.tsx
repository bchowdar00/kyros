import { ScrollFade } from '@kyros/ui/components/scroll-fade'
import { SectionHeader } from '@kyros/ui/components/section-header'
import { MiniForwardIcon } from '@kyros/ui/icons/mini-forward-icon'

import { SignalCard } from './signal-card'
import type { SignalsData } from './types'

export type SignalsSectionProps = SignalsData & {
  /** Opens the full list; without it the control is inert (static fixtures). */
  onBrowse?: () => void
  /** Opens one card's detail view; without it the cards are static readings. */
  onOpen?: (id: string, trigger: HTMLElement) => void
}

export function SignalsSection({
  browseLabel,
  items,
  onBrowse,
  onOpen,
  title,
  total,
}: SignalsSectionProps) {
  return (
    <section aria-label={title} className="flex h-[134px] flex-col gap-xl" id="signals">
      <SectionHeader
        action={
          <button
            aria-label={`${browseLabel} (${total} signals)`}
            className="flex h-[18px] items-center gap-sm rounded-xs text-heading-sm font-regular text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-focus-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            onClick={onBrowse}
            type="button"
          >
            <span>
              {browseLabel} ({total})
            </span>
            <MiniForwardIcon className="size-[18px]" />
          </button>
        }
        className="h-[18px]"
        title={title}
      />

      <div className="relative min-w-0 flex-1">
        <section
          aria-label={`${title} list`}
          className="size-full overflow-x-auto rounded-sm outline-none [scrollbar-width:none] focus-visible:ring-2 focus-visible:ring-focus-ring focus-visible:ring-inset [&::-webkit-scrollbar]:hidden"
        >
          <ul className="flex w-max min-w-full gap-md">
            {items.map(signal => (
              <li className="shrink-0" key={signal.id}>
                <SignalCard onOpen={onOpen} signal={signal} />
              </li>
            ))}
          </ul>
        </section>

        <ScrollFade className="w-[30px] from-background/85" />
      </div>
    </section>
  )
}
