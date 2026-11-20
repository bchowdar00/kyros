import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { useIdentityStore } from './identity-store'
import { ProfileMenu } from './profile-menu'

const identity = {
  address: `0x${'ab'.repeat(20)}`,
  color: '#7c5cff',
  emoji: '🦉',
  username: 'guest-4aa3r9',
}

describe('ProfileMenu', () => {
  const createGuest = vi.fn(() => Promise.resolve())

  beforeEach(() => {
    createGuest.mockClear()
    useIdentityStore.setState({ createGuest, identity })
  })

  it('lets a visitor start an account under a name of their own', async () => {
    const user = userEvent.setup()
    render(<ProfileMenu apiUrl="https://exchange.test" identity={identity} />)

    await user.type(screen.getByRole('textbox', { hidden: true, name: 'Your name' }), 'Night Owl')
    await user.click(screen.getByRole('button', { hidden: true, name: 'Use this name' }))

    expect(createGuest).toHaveBeenCalledWith('https://exchange.test', 'Night Owl')
    // It is a new account, and the menu says so before the visitor presses anything.
    expect(screen.getByText(/Starts a new paper account under this name/)).toBeInTheDocument()
  })

  it('does nothing for a blank name', async () => {
    const user = userEvent.setup()
    render(<ProfileMenu apiUrl="https://exchange.test" identity={identity} />)

    await user.click(screen.getByRole('button', { hidden: true, name: 'Use this name' }))

    expect(createGuest).not.toHaveBeenCalled()
  })
})
