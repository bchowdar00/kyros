/**
 * The subset of Markdown a reply may carry: headings, paragraphs, bullet and numbered
 * lists, and inline bold, emphasis and code. Nothing is ever turned into HTML; the
 * parser returns a tree the renderer maps to elements, so model output cannot inject.
 */

export type Inline =
  | { kind: 'text'; text: string }
  | { kind: 'bold'; text: string }
  | { kind: 'em'; text: string }
  | { kind: 'code'; text: string }

export type Block =
  | { kind: 'heading'; level: 1 | 2 | 3; inlines: Inline[] }
  | { kind: 'paragraph'; inlines: Inline[] }
  | { kind: 'list'; ordered: boolean; items: Inline[][] }
  | { kind: 'quote'; inlines: Inline[] }
  | { kind: 'code'; language: string | undefined; text: string }
  | { kind: 'rule' }

/**
 * Untrusted strings are cut before any regex runs (AGENTS.md §11). The cut applies to each
 * line and to each run of inline text, not to the whole reply, so a long answer is not
 * truncated. The source itself is bounded only to keep block count finite.
 */
const MAX_REGEX_INPUT = 2_000
const MAX_SOURCE = 40_000
/** A link keeps its text and drops its target; the sources widget is where links live. */
const LINK = /\[([^\]]+)\]\([^)\s]*\)/g
const HEADING = /^(#{1,3})\s+(.*)$/
const BULLET = /^[-*]\s+(.*)$/
const NUMBERED = /^\d+[.)]\s+(.*)$/
const QUOTE = /^>\s?(.*)$/
const RULE = /^(?:-{3,}|\*{3,}|_{3,})$/
const FENCE = /^```\s*([\w+-]*)\s*$/

/**
 * Inline marks, in the order they are tried at a position. Bold and code with no closer
 * run to the end of the text: while a reply streams, "**Edge" is bold-in-progress, not
 * two asterisks. A lone `*` with no closer stays literal because it is usually arithmetic.
 */
const MARKS: readonly {
  open: string
  kind: Exclude<Inline, { kind: 'text' }>['kind']
  toEnd: boolean
}[] = [
  { open: '***', kind: 'bold', toEnd: true },
  { open: '**', kind: 'bold', toEnd: true },
  { open: '`', kind: 'code', toEnd: true },
  { open: '*', kind: 'em', toEnd: false },
]

function pushText(inlines: Inline[], text: string) {
  if (text === '') return
  const last = inlines.at(-1)
  if (last?.kind === 'text') last.text += text
  else inlines.push({ kind: 'text', text })
}

export function parseInline(source: string): Inline[] {
  const text = source.slice(0, MAX_REGEX_INPUT).replace(LINK, '$1')
  const inlines: Inline[] = []
  let cursor = 0
  let plainFrom = 0
  while (cursor < text.length) {
    const mark = MARKS.find(candidate => text.startsWith(candidate.open, cursor))
    const after = mark ? text[cursor + mark.open.length] : undefined
    if (!mark || after === undefined || after === ' ' || after === mark.open[0]) {
      cursor += 1
      continue
    }
    const contentStart = cursor + mark.open.length
    const closeAt = text.indexOf(mark.open, contentStart)
    if (closeAt === -1 && !mark.toEnd) {
      cursor += 1
      continue
    }
    pushText(inlines, text.slice(plainFrom, cursor))
    const content = closeAt === -1 ? text.slice(contentStart) : text.slice(contentStart, closeAt)
    inlines.push({ kind: mark.kind, text: content })
    cursor = closeAt === -1 ? text.length : closeAt + mark.open.length
    plainFrom = cursor
  }
  pushText(inlines, text.slice(plainFrom))
  return inlines
}

/**
 * A block still being collected. Text is inline-parsed only when the block closes, so a
 * list item or paragraph that wraps onto several source lines is one run of text.
 */
type Open =
  | { kind: 'paragraph'; lines: string[] }
  | { kind: 'quote'; lines: string[] }
  | { kind: 'list'; ordered: boolean; items: string[] }
  | { kind: 'code'; language: string | undefined; lines: string[] }

function close(open: Open | null, blocks: Block[]) {
  if (!open) return
  switch (open.kind) {
    case 'paragraph':
      blocks.push({ kind: 'paragraph', inlines: parseInline(open.lines.join(' ')) })
      return
    case 'quote':
      blocks.push({ kind: 'quote', inlines: parseInline(open.lines.join(' ')) })
      return
    case 'list':
      blocks.push({
        kind: 'list',
        ordered: open.ordered,
        items: open.items.map(item => parseInline(item)),
      })
      return
    case 'code':
      blocks.push({ kind: 'code', language: open.language, text: open.lines.join('\n') })
      return
  }
}

export function parseMarkdown(source: string): Block[] {
  const blocks: Block[] = []
  const lines = source.slice(0, MAX_SOURCE).replaceAll('\r\n', '\n').split('\n')
  let open: Open | null = null

  for (const sourceLine of lines) {
    const raw = sourceLine.slice(0, MAX_REGEX_INPUT)
    const line = raw.trim()
    // Inside a fence every line is verbatim until the closing fence; an unclosed fence
    // (the reply is still streaming) runs to the end of the text.
    if (open?.kind === 'code') {
      if (FENCE.test(line)) {
        close(open, blocks)
        open = null
      } else open.lines.push(raw.trimEnd())
      continue
    }
    if (line === '') {
      close(open, blocks)
      open = null
      continue
    }
    const fence = FENCE.exec(line)
    if (fence) {
      close(open, blocks)
      open = { kind: 'code', language: fence[1] || undefined, lines: [] }
      continue
    }
    if (RULE.test(line)) {
      close(open, blocks)
      open = null
      blocks.push({ kind: 'rule' })
      continue
    }
    const quote = QUOTE.exec(line)?.[1]
    if (quote !== undefined) {
      if (open?.kind === 'quote') open.lines.push(quote)
      else {
        close(open, blocks)
        open = { kind: 'quote', lines: [quote] }
      }
      continue
    }
    const heading = HEADING.exec(line)
    if (heading) {
      close(open, blocks)
      open = null
      const level = Math.min(3, heading[1]?.length ?? 1) as 1 | 2 | 3
      blocks.push({ kind: 'heading', level, inlines: parseInline(heading[2] ?? '') })
      continue
    }
    const bullet = BULLET.exec(line)?.[1]
    const numbered = NUMBERED.exec(line)?.[1]
    const marker = bullet ?? numbered
    if (marker !== undefined) {
      const ordered = bullet === undefined
      if (open?.kind === 'list' && open.ordered === ordered) {
        open.items.push(marker)
      } else {
        close(open, blocks)
        open = { kind: 'list', ordered, items: [marker] }
      }
      continue
    }
    // An indented line directly under a list item is that item wrapping onto the next line;
    // a flush-left line is a new paragraph and ends the list.
    if (open?.kind === 'list') {
      if (raw.trimStart().length < raw.length) {
        const last = open.items.length - 1
        open.items[last] = `${open.items[last] ?? ''} ${line}`
        continue
      }
    }
    if (open && open.kind !== 'paragraph') {
      close(open, blocks)
      open = null
    }
    if (open) open.lines.push(line)
    else open = { kind: 'paragraph', lines: [line] }
  }
  close(open, blocks)
  return blocks
}
