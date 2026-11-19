import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import {
  CodeExample,
  DiffExample,
  MarkdownExample,
  SourcesExample,
} from '@/features/chat/examples/content-examples'
import { parseInline, parseMarkdown } from '@/features/chat/widgets/markdown'

describe('parseMarkdown', () => {
  it('splits headings, lists and paragraphs and keeps inline marks', () => {
    const blocks = parseMarkdown('# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n\n1. first')
    expect(blocks.map(block => block.kind)).toEqual(['heading', 'paragraph', 'list', 'list'])
    expect(blocks[1]).toEqual({
      kind: 'paragraph',
      inlines: [
        { kind: 'text', text: 'Some ' },
        { kind: 'bold', text: 'bold' },
        { kind: 'text', text: ' and ' },
        { kind: 'code', text: 'code' },
        { kind: 'text', text: '.' },
      ],
    })
    expect(blocks[3]).toMatchObject({ kind: 'list', ordered: true })
  })

  it('never produces markup from angle brackets', () => {
    const [block] = parseMarkdown('<script>alert(1)</script>')
    expect(block).toEqual({
      kind: 'paragraph',
      inlines: [{ kind: 'text', text: '<script>alert(1)</script>' }],
    })
  })

  it('joins an indented continuation into its list item and ends the list at flush-left text', () => {
    const blocks = parseMarkdown('Intro:\n- one that\n  wraps\n- two\nAfter.')
    expect(blocks.map(block => block.kind)).toEqual(['paragraph', 'list', 'paragraph'])
    expect(blocks[1]).toEqual({
      kind: 'list',
      ordered: false,
      items: [[{ kind: 'text', text: 'one that wraps' }], [{ kind: 'text', text: 'two' }]],
    })
  })

  it('switches list kind when the marker changes', () => {
    const blocks = parseMarkdown('- a\n- b\n1. c\n2. d')
    expect(blocks).toMatchObject([
      { kind: 'list', ordered: false },
      { kind: 'list', ordered: true },
    ])
  })

  it('keeps fenced code verbatim and runs an unclosed fence to the end', () => {
    const closed = parseMarkdown('```json\n{ "a": 1 }\n  in\n```\nafter')
    expect(closed[0]).toEqual({ kind: 'code', language: 'json', text: '{ "a": 1 }\n  in' })
    expect(closed[1]).toMatchObject({ kind: 'paragraph' })
    const open = parseMarkdown('```\n**not bold**\nstill')
    expect(open).toEqual([{ kind: 'code', language: undefined, text: '**not bold**\nstill' }])
  })

  it('parses rules and quotes', () => {
    const blocks = parseMarkdown('> a quote\n> continues\n\n---\n\nafter')
    expect(blocks).toEqual([
      { kind: 'quote', inlines: [{ kind: 'text', text: 'a quote continues' }] },
      { kind: 'rule' },
      { kind: 'paragraph', inlines: [{ kind: 'text', text: 'after' }] },
    ])
  })

  it('drops link targets, folds bold-italic into bold and leaves arithmetic alone', () => {
    expect(parseInline('See [the docs](https://x.y/z) now')).toEqual([
      { kind: 'text', text: 'See the docs now' },
    ])
    expect(parseInline('A ***strong*** word')).toEqual([
      { kind: 'text', text: 'A ' },
      { kind: 'bold', text: 'strong' },
      { kind: 'text', text: ' word' },
    ])
    expect(parseInline('5 * 3 = 15 and 2 ** 3')).toEqual([
      { kind: 'text', text: '5 * 3 = 15 and 2 ** 3' },
    ])
  })

  it('treats an unclosed bold or code opener as running to the end while streaming', () => {
    expect(parseInline('Streaming **Edge by regi')).toEqual([
      { kind: 'text', text: 'Streaming ' },
      { kind: 'bold', text: 'Edge by regi' },
    ])
    expect(parseInline('Unclosed `code')).toEqual([
      { kind: 'text', text: 'Unclosed ' },
      { kind: 'code', text: 'code' },
    ])
    expect(parseInline('a lone *star')).toEqual([{ kind: 'text', text: 'a lone *star' }])
  })

  it('does not truncate a long reply', () => {
    const paragraph = 'word '.repeat(100).trim()
    const source = Array.from({ length: 12 }, (_, index) => `P${index} ${paragraph}`).join('\n\n')
    expect(source.length).toBeGreaterThan(6_000)
    const blocks = parseMarkdown(source)
    expect(blocks).toHaveLength(12)
    expect(blocks[11]).toMatchObject({ kind: 'paragraph' })
  })
})

describe('CodeBlock widget', () => {
  it('copies the code and confirms', async () => {
    const user = userEvent.setup()
    const { container } = render(<CodeExample />)
    await user.click(screen.getByRole('button', { name: 'Copy code' }))
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeVisible()
    expect(await navigator.clipboard.readText()).toContain('"series": "btc-5m"')
    await expectNoA11yViolations(container)
  })
})

describe('Sources widget', () => {
  it('links allowlisted hosts safely and numbers every citation', async () => {
    const { container } = render(<SourcesExample />)
    const list = within(screen.getByRole('region', { name: 'Sources for the price answer' }))
    expect(list.getAllByRole('listitem')).toHaveLength(3)
    const link = list.getByRole('link', { name: /Pyth Lazer/ })
    expect(link).toHaveAttribute('rel', 'noopener noreferrer nofollow ugc')
    expect(list.getByText('docs.pyth.network · read 2:33 PM')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})

describe('Diff widget', () => {
  it('counts additions and removals and announces each changed line', async () => {
    const { container } = render(<DiffExample />)
    const frame = within(screen.getByRole('region', { name: 'Strategy edit' }))
    expect(frame.getByText('+3')).toBeVisible()
    expect(frame.getByText('−1')).toBeVisible()
    expect(frame.getByText('Removed', { exact: false })).toBeInTheDocument()
    expect(frame.getAllByRole('listitem')).toHaveLength(6)
    await expectNoA11yViolations(container)
  })
})

describe('MarkdownBlock widget', () => {
  it('renders lists and emphasis as real elements', async () => {
    const { container } = render(<MarkdownExample />)
    const frame = screen.getByRole('region', { name: 'Strategy notes' })
    expect(within(frame).getAllByRole('list')).toHaveLength(2)
    expect(within(frame).getByText('9/21 EMA cross').tagName).toBe('STRONG')
    expect(container.querySelector('script')).toBeNull()
    await expectNoA11yViolations(container)
  })
})
