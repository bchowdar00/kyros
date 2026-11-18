import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import type { UIMessage } from 'ai'
import { describe, expect, it, vi } from 'vitest'
import { ChatMessages } from '@/features/market-detail/chat-messages'
import { openQuestion } from '@/features/market-detail/open-question'
import { QuestionCard } from '@/features/market-detail/question-card'
import type { AskQuestionOutput } from '@/mastra/capabilities/strategy/question-catalogue'

const question: AskQuestionOutput = {
  status: 'awaiting_answer',
  kind: 'side',
  question: 'Which side should it buy?',
  options: [
    { label: 'Up', reason: '' },
    { label: 'Down', reason: '' },
    { label: 'Whichever side qualifies', reason: 'The rule picks the side when it fires' },
  ],
  recommended: 2,
}

function asked(id: string, output: unknown = question): UIMessage {
  return {
    id,
    role: 'assistant',
    parts: [
      { type: 'text', text: 'One thing before I plan it.' },
      {
        type: 'tool-askQuestion',
        toolCallId: `call-${id}`,
        state: 'output-available',
        input: {},
        output,
      },
    ],
  } as UIMessage
}

const user = (id: string, text: string): UIMessage => ({
  id,
  role: 'user',
  parts: [{ type: 'text', text }],
})
const noop = () => {}

describe('QuestionCard', () => {
  it('shows the recommendation first and answers with a click', async () => {
    const onAnswer = vi.fn()
    const { container } = render(
      <QuestionCard question={question} onAnswer={onAnswer} onDismiss={vi.fn()} />
    )
    expect(screen.getByRole('heading', { name: 'Teeny needs a decision' })).toBeInTheDocument()
    expect(screen.getAllByText('Which side should it buy?').length).toBeGreaterThan(0)
    const buttons = screen.getAllByRole('button')
    expect(buttons[0]).toHaveAccessibleName(/Whichever side qualifies.*Recommended/)
    await userEvent.click(screen.getByRole('button', { name: /^Down$/ }))
    expect(onAnswer).toHaveBeenCalledWith('Down')
    await expectNoA11yViolations(container)
  })

  it('moves with the arrows, answers with Enter and a digit, dismisses with Escape', async () => {
    const onAnswer = vi.fn()
    const onDismiss = vi.fn()
    render(<QuestionCard question={question} onAnswer={onAnswer} onDismiss={onDismiss} />)
    const first = screen.getByRole('button', { name: /Whichever side qualifies/ })
    first.focus()
    await userEvent.keyboard('{ArrowDown}')
    expect(screen.getByRole('button', { name: /^Up$/ })).toHaveFocus()
    await userEvent.keyboard('{Enter}')
    expect(onAnswer).toHaveBeenLastCalledWith('Up')
    await userEvent.keyboard('2')
    expect(onAnswer).toHaveBeenLastCalledWith('Up')
    await userEvent.keyboard('3')
    expect(onAnswer).toHaveBeenLastCalledWith('Down')
    await userEvent.keyboard('{Escape}')
    expect(onDismiss).toHaveBeenCalledTimes(1)
  })

  it('opens a text field for a typed answer and sends it', async () => {
    const onAnswer = vi.fn()
    const onDismiss = vi.fn()
    const { container } = render(
      <QuestionCard question={question} onAnswer={onAnswer} onDismiss={onDismiss} />
    )
    expect(screen.queryByRole('textbox', { name: 'Your answer' })).not.toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: /Something else/ }))
    const field = screen.getByRole('textbox', { name: 'Your answer' })
    expect(field).toHaveFocus()
    expect(screen.getByRole('button', { name: 'Send answer' })).toBeDisabled()
    await userEvent.keyboard('Whichever side is cheaper{Enter}')
    expect(onAnswer).toHaveBeenCalledWith('Whichever side is cheaper')
    expect(onDismiss).not.toHaveBeenCalled()
    await expectNoA11yViolations(container)
    // Escape in the field goes back to the choices instead of closing the card.
    await userEvent.keyboard('{Escape}')
    expect(screen.queryByRole('textbox', { name: 'Your answer' })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Something else/ })).toHaveFocus()
    expect(onDismiss).not.toHaveBeenCalled()
  })
})

describe('openQuestion', () => {
  it('is the newest question of a trailing assistant message that is not dismissed', () => {
    expect(openQuestion([user('u1', 'build it'), asked('a1')], new Set())?.id).toBe('call-a1')
    expect(openQuestion([asked('a1'), user('u2', 'Up')], new Set())).toBeUndefined()
    expect(
      openQuestion([user('u1', 'build it'), asked('a1')], new Set(['call-a1']))
    ).toBeUndefined()
    expect(
      openQuestion([asked('a1', { status: 'invalid', problem: 'x' })], new Set())
    ).toBeUndefined()
  })
})

describe('ChatMessages with a question', () => {
  it('keeps an open question out of the log and records an answered one', () => {
    const { rerender } = render(
      <ChatMessages
        messages={[user('u1', 'build it'), asked('a1')]}
        pending={false}
        failed={false}
        onRetry={noop}
      />
    )
    expect(screen.queryByText('Which side should it buy?')).not.toBeInTheDocument()
    rerender(
      <ChatMessages
        messages={[user('u1', 'build it'), asked('a1'), user('u2', 'Down')]}
        pending={false}
        failed={false}
        onRetry={noop}
      />
    )
    expect(screen.getByText('Which side should it buy?')).toBeInTheDocument()
    expect(screen.getByText('Answered:')).toBeInTheDocument()
    expect(screen.getAllByText('Down')).toHaveLength(2)
  })
})
