import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { createRef } from 'react'
import { describe, expect, it } from 'vitest'

import { Button } from '../components/button'
import { IconButton } from '../components/icon-button'
import { ScrollFade } from '../components/scroll-fade'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '../components/tabs'
import { ShareIcon } from '../icons/share-icon'

describe('Button', () => {
  it('uses safe button semantics and forwards its ref', async () => {
    const ref = createRef<HTMLButtonElement>()

    const { container } = render(<Button ref={ref}>Deploy</Button>)

    const button = screen.getByRole('button', { name: 'Deploy' })

    expect(button).toHaveAttribute('type', 'button')
    expect(ref.current).toBe(button)
    await expectNoA11yViolations(container)
  })

  it('composes a link without adding button semantics', () => {
    render(
      <Button asChild>
        <a href="https://example.com/market">Open market</a>
      </Button>
    )

    const link = screen.getByRole('link', { name: 'Open market' })

    expect(link).toHaveAttribute('href', 'https://example.com/market')
    expect(link).not.toHaveAttribute('type')
  })
})

describe('IconButton', () => {
  it('requires an accessible name while keeping its icon decorative', async () => {
    const { container } = render(
      <IconButton aria-label="Share market">
        <ShareIcon data-testid="share-icon" />
      </IconButton>
    )

    expect(screen.getByRole('button', { name: 'Share market' })).toBeInTheDocument()
    expect(screen.getByTestId('share-icon')).toHaveAttribute('aria-hidden', 'true')
    await expectNoA11yViolations(container)
  })
})

describe('Tabs', () => {
  it('supports automatic keyboard activation', async () => {
    const user = userEvent.setup()

    const { container } = render(
      <Tabs defaultValue="active">
        <TabsList aria-label="Strategies">
          <TabsTrigger value="active">Active</TabsTrigger>
          <TabsTrigger value="paused">Paused</TabsTrigger>
        </TabsList>
        <TabsContent value="active">Active strategies</TabsContent>
        <TabsContent value="paused">Paused strategies</TabsContent>
      </Tabs>
    )

    const activeTab = screen.getByRole('tab', { name: 'Active' })
    const pausedTab = screen.getByRole('tab', { name: 'Paused' })

    expect(activeTab).toHaveAttribute('aria-selected', 'true')
    expect(screen.getByRole('tabpanel')).toHaveTextContent('Active strategies')
    await expectNoA11yViolations(container)

    await user.tab()
    expect(activeTab).toHaveFocus()

    await user.keyboard('{ArrowRight}')
    expect(pausedTab).toHaveFocus()
    expect(pausedTab).toHaveAttribute('aria-selected', 'true')
    expect(screen.getByRole('tabpanel')).toHaveTextContent('Paused strategies')
  })
})

describe('ScrollFade', () => {
  it('stays out of the accessibility tree and accepts no override of that', async () => {
    const { container } = render(
      <div>
        <p>Scrollable content</p>
        <ScrollFade data-testid="fade" edge="bottom" strength="strong" />
      </div>
    )

    expect(screen.getByTestId('fade')).toHaveAttribute('aria-hidden', 'true')
    await expectNoA11yViolations(container)
  })
})
