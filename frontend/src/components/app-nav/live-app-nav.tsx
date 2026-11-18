'use client'

import { cents, createMoneyFormatter } from '@tiny/money'

import { useIdentityStore } from '@/features/identity/identity-store'
import { ProfileMenu } from '@/features/identity/profile-menu'

import { AppNav, type AppNavProps } from './app-nav'

const money = createMoneyFormatter('en-US')
const TENTHS_PER_CENT = 10

/**
 * The shell's account summary follows the exchange account once one exists. Before that,
 * and on the server, it renders the placeholder so hydration matches.
 */
export function LiveAppNav(props: AppNavProps) {
  const identity = useIdentityStore(state => state.identity)
  const balanceTenths = useIdentityStore(state => state.balanceTenths)

  if (identity === null) {
    return <AppNav {...props} />
  }

  const balance =
    balanceTenths === null ? '—' : money.usd(cents(Math.round(balanceTenths / TENTHS_PER_CENT)))

  return (
    <AppNav
      {...props}
      balance={balance}
      balanceLabel={
        balanceTenths === null ? 'Exchange balance unknown' : `Exchange balance: ${balance}`
      }
      profileMenu={<ProfileMenu identity={identity} />}
    />
  )
}
