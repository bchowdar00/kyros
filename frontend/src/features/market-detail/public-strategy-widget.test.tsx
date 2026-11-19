import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { describe, expect, it } from 'vitest'

import { cents } from '@kyros/money'

import { ChatPanel } from './chat-panel'
import type { StrategyDeploy, StrategyDeployRequest } from './exchange/deploy'
import { MarketBuyButton } from './market-buy-button'
import { PUBLIC_STRATEGY_FIXTURES } from './public-strategies.fixture'
import type { PublicStrategy } from './types'

function renderStrategies(
  marketKey: string,
  deploy?: StrategyDeploy,
  publicStrategies: readonly PublicStrategy[] = PUBLIC_STRATEGY_FIXTURES
) {
  return render(
    <>
      <MarketBuyButton marketKey={marketKey} side="home">
        Buy UP
      </MarketBuyButton>
      <ChatPanel
        marketKey={marketKey}
        deploy={deploy}
        scopeLabel="BTC Up or Down 5m"
        outcomes={[{ label: 'UP', side: 'home', priceCents: 62 }]}
        data={{ view: 'strategies', publicStrategies }}
      />
    </>
  )
}

const liveDeploy: StrategyDeploy = {
  availableBalanceCents: cents(100_000),
  reason: undefined,
  notice: '',
  submit: () => Promise.resolve(true),
}

describe('public strategy widget', () => {
  it.each(['Run', 'Inverse'])(
    'opens %s with the selected logic and restores the draft on Escape',
    async action => {
      const user = userEvent.setup()
      const { container } = renderStrategies(`strategy-${action}`)
      const strategy = PUBLIC_STRATEGY_FIXTURES[0]
      const composer = screen.getByRole('textbox', { name: 'Message Kyros' })
      await user.type(composer, 'Keep this draft')
      const trigger = screen.getByRole('button', { name: `${action} ${strategy.title}` })
      await user.click(trigger)
      const widget = screen.getByRole('dialog', { name: 'Public strategy' })
      await waitFor(() =>
        expect(
          within(widget).getByText(action === 'Run' ? strategy.logic : strategy.inverseLogic)
        ).toBeVisible()
      )
      expect(
        within(widget).getByRole('heading', {
          name: action === 'Run' ? strategy.title : `Inverse: ${strategy.title}`,
        })
      ).toBeVisible()
      expect(
        within(widget).getByRole('button', { name: 'Back to strategy composer' })
      ).toHaveFocus()
      expect(screen.queryByRole('form', { name: 'Strategy composer' })).not.toBeInTheDocument()
      await expectNoA11yViolations(container)
      await user.keyboard('{Escape}')
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
      expect(composer).toHaveValue('Keep this draft')
      expect(trigger).toHaveFocus()
    }
  )

  it('switches logic tabs with mouse and keyboard while retaining the allocation', async () => {
    const user = userEvent.setup()
    renderStrategies('logic-tabs')
    const strategy = PUBLIC_STRATEGY_FIXTURES[1]
    await user.click(screen.getByRole('button', { name: `Run ${strategy.title}` }))
    const allocation = screen.getByRole('textbox', { name: 'Total allocation in USD' })
    await user.clear(allocation)
    await user.type(allocation, '250.50')
    const nodes = screen.getByRole('tab', { name: 'Nodes' })
    await user.click(nodes)
    expect(nodes).toHaveAttribute('aria-selected', 'true')
    expect(screen.getByRole('tabpanel', { name: 'Nodes' })).toBeVisible()
    expect(screen.queryByRole('tabpanel', { name: 'English' })).not.toBeInTheDocument()
    await user.keyboard('{ArrowLeft}')
    expect(screen.getByRole('tab', { name: 'English' })).toHaveFocus()
    expect(screen.getByRole('tabpanel', { name: 'English' })).toHaveTextContent(strategy.logic)
    expect(allocation).toHaveValue('250.50')
    expect(screen.getByRole('combobox', { name: 'Scope' })).toHaveValue('BTC Up or Down 5m')
    for (const name of ['View Advanced Settings', 'Remix']) {
      const button = screen.getByRole('button', { name })
      expect(button).toBeEnabled()
      await user.click(button)
      expect(screen.getByRole('dialog', { name: 'Public strategy' })).toBeVisible()
    }
    // Without an exchange behind the page there is nothing to deploy into.
    expect(screen.getByRole('button', { name: 'Deploy Strategy' })).toBeDisabled()
    expect(screen.getByText('Connect to an exchange to deploy.')).toBeVisible()
  })

  it('deploys the chosen side with the allocation in cents and reports the outcome', async () => {
    const user = userEvent.setup()
    const requests: StrategyDeployRequest[] = []
    const deploy: StrategyDeploy = {
      availableBalanceCents: cents(100_000),
      reason: undefined,
      notice: '',
      submit: request => {
        requests.push(request)
        return Promise.resolve(true)
      },
    }
    renderStrategies('deploy', deploy)
    const strategy = PUBLIC_STRATEGY_FIXTURES[0]
    await user.click(screen.getByRole('button', { name: `Inverse ${strategy.title}` }))
    const button = screen.getByRole('button', { name: 'Deploy Strategy' })
    expect(button).toBeEnabled()
    const allocation = screen.getByRole('textbox', { name: 'Total allocation in USD' })
    await user.clear(allocation)
    await user.type(allocation, '9.99')
    expect(button).toBeDisabled()
    expect(screen.getByText('Enter an allocation of at least $10.')).toBeVisible()
    await user.clear(allocation)
    await user.type(allocation, '1000.01')
    expect(screen.getByText('Allocation exceeds your available balance.')).toBeVisible()
    await user.clear(allocation)
    await user.type(allocation, '25.5')
    expect(button).toBeEnabled()
    await user.click(button)
    expect(requests).toEqual([
      { templateId: strategy.templateId, inverse: true, allocationCents: 2_550 },
    ])
  })

  it('shows why the account cannot deploy and the last deploy notice', async () => {
    const user = userEvent.setup()
    renderStrategies('deploy-blocked', {
      availableBalanceCents: undefined,
      reason: 'Waiting for the exchange account snapshot.',
      notice: 'Not deployed. Insufficient balance.',
      submit: () => Promise.resolve(false),
    })
    await user.click(
      screen.getByRole('button', { name: `Run ${PUBLIC_STRATEGY_FIXTURES[1].title}` })
    )
    expect(screen.getByRole('button', { name: 'Deploy Strategy' })).toBeDisabled()
    await waitFor(() =>
      expect(screen.getByText('Waiting for the exchange account snapshot.')).toBeVisible()
    )
    expect(screen.getByRole('status')).toHaveTextContent('Not deployed. Insufficient balance.')
  })

  it('refuses to deploy a strategy whose signals the exchange is still waiting on', async () => {
    const user = userEvent.setup()
    const waiting: PublicStrategy = {
      ...PUBLIC_STRATEGY_FIXTURES[0],
      waitingOn: ['BTC fifteen-minute momentum'],
    }
    renderStrategies('deploy-waiting', liveDeploy, [waiting])
    await user.click(screen.getByRole('button', { name: `Run ${waiting.title}` }))
    expect(screen.getByRole('button', { name: 'Deploy Strategy' })).toBeDisabled()
    await waitFor(() =>
      expect(
        screen.getByText(
          'Waiting on BTC fifteen-minute momentum. Deploying now would leave the strategy idle.'
        )
      ).toBeVisible()
    )
  })

  it('dismisses with back or the scrim, and can switch to the buy widget', async () => {
    const user = userEvent.setup()
    renderStrategies('strategy-dismiss')
    const trigger = screen.getByRole('button', { name: `Run ${PUBLIC_STRATEGY_FIXTURES[0].title}` })
    for (const name of ['Back to strategy composer', 'Close public strategy']) {
      await user.click(trigger)
      await user.click(screen.getByRole('button', { name }))
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
      expect(trigger).toHaveFocus()
    }
    await user.click(trigger)
    await user.click(screen.getByRole('button', { name: 'Buy UP' }))
    expect(screen.queryByRole('dialog', { name: 'Public strategy' })).not.toBeInTheDocument()
    expect(screen.getByRole('dialog', { name: 'Buy outcome' })).toBeVisible()
    expect(screen.getByRole('button', { name: 'Select UP' })).toHaveAttribute(
      'aria-pressed',
      'true'
    )
  })
})
