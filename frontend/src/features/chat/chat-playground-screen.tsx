import { GALLERY_SECTIONS, type GalleryEntry } from './gallery-entries'
import { WidgetFrame } from './widget-frame'

/**
 * A gallery of every chat widget at the size it renders inside a reply. The frame
 * carries its own width, so a component designed here drops into a conversation
 * without changing size. Everything shown is sample data (AGENTS.md §0.4 waiver, as
 * on /charts); the route is unindexed.
 */
export function ChatPlaygroundScreen() {
  return (
    <main className="mx-auto flex w-full max-w-[90rem] flex-col gap-4xl px-4xl py-xl pb-4xl">
      <div className="flex flex-col gap-copy">
        <h1 className="text-heading-md font-regular text-foreground">Chat widgets</h1>
        <p className="text-label font-medium text-muted-foreground">
          Sample data. Every frame is the size a widget has inside a chat reply.
        </p>
      </div>
      {GALLERY_SECTIONS.map(section => (
        <section
          key={section.id}
          aria-labelledby={`gallery-${section.id}`}
          className="flex flex-col gap-xl"
        >
          <div className="flex flex-col gap-copy">
            <h2
              id={`gallery-${section.id}`}
              className="text-heading-sm font-regular text-foreground"
            >
              {section.title}
            </h2>
            <p className="text-label text-muted-foreground">{section.description}</p>
          </div>
          <ul
            aria-label={`${section.title} widgets`}
            className="grid list-none grid-cols-[repeat(auto-fill,var(--spacing-chat-widget))] items-start justify-start gap-xl"
          >
            {section.entries.map(entry => (
              <li key={entry.id} className="flex">
                <GallerySlot entry={entry} />
              </li>
            ))}
          </ul>
        </section>
      ))}
    </main>
  )
}

function GallerySlot({ entry }: { entry: GalleryEntry }) {
  if (entry.render) return entry.render()
  return (
    <WidgetFrame label={entry.title} title={null} className="min-h-chat-widget-min">
      <p className="m-auto text-label text-muted-foreground">{entry.title}</p>
    </WidgetFrame>
  )
}
