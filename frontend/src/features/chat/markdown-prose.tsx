import { type Block, type Inline, parseMarkdown } from './widgets/markdown'

function Inlines({ inlines }: { inlines: readonly Inline[] }) {
  return inlines.map((inline, index) => {
    switch (inline.kind) {
      case 'bold':
        return (
          <strong key={index} className="font-medium text-foreground">
            {inline.text}
          </strong>
        )
      case 'em':
        return <em key={index}>{inline.text}</em>
      case 'code':
        return (
          <code key={index} className="rounded-xs bg-surface-raised px-xs font-mono text-label">
            {inline.text}
          </code>
        )
      default:
        return inline.text
    }
  })
}

function BlockView({ block }: { block: Block }) {
  switch (block.kind) {
    case 'heading':
      // Every level is an h4: a widget's title is the h3, and a reply never nests deeper.
      // The first heading in a run gets no extra top margin from the block gap alone.
      return (
        <h4
          className={
            block.level === 1
              ? 'mt-sm text-heading-sm font-medium text-foreground'
              : 'mt-sm text-body font-medium text-foreground'
          }
        >
          <Inlines inlines={block.inlines} />
        </h4>
      )
    case 'list': {
      const List = block.ordered ? 'ol' : 'ul'
      return (
        <List
          className={
            block.ordered
              ? 'flex list-decimal flex-col gap-xs pl-lg text-body leading-prose text-foreground marker:text-muted-foreground'
              : 'flex list-disc flex-col gap-xs pl-lg text-body leading-prose text-foreground marker:text-muted-foreground'
          }
        >
          {block.items.map((item, index) => (
            <li key={index} className="pl-xs">
              <Inlines inlines={item} />
            </li>
          ))}
        </List>
      )
    }
    case 'quote':
      return (
        <blockquote className="border-l-2 border-border-subtle pl-md text-body leading-prose text-muted-foreground">
          <Inlines inlines={block.inlines} />
        </blockquote>
      )
    case 'code':
      return (
        <pre className="overflow-x-auto rounded-sm bg-surface-raised px-md py-sm font-mono text-label leading-relaxed text-foreground">
          <code>{block.text}</code>
        </pre>
      )
    case 'rule':
      return <hr className="border-t border-border-subtle/50" />
    default:
      return (
        <p className="text-body leading-prose text-foreground">
          <Inlines inlines={block.inlines} />
        </p>
      )
  }
}

/**
 * Markdown from the model rendered as chat prose: the parsed tree mapped to elements,
 * never HTML. Used for reply text directly and inside the markdown widget's frame.
 */
export function MarkdownProse({ source }: { source: string }) {
  const blocks = parseMarkdown(source)
  return (
    <div className="flex flex-col gap-md">
      {blocks.map((block, index) => (
        <BlockView key={index} block={block} />
      ))}
    </div>
  )
}
