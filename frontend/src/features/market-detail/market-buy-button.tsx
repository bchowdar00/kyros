'use client'

import { Button, type ButtonProps } from '@kyros/ui/components/button'
import { useId } from 'react'

import type { MarketSide } from './types'
import { useTradeTicket } from './use-trade-ticket'

interface MarketBuyButtonProps extends ButtonProps {
  marketKey: string
  side: MarketSide
}

export function MarketBuyButton({ marketKey, side, ...props }: MarketBuyButtonProps) {
  const { ticket } = useTradeTicket(marketKey)
  const triggerId = useId()
  return <Button {...props} id={triggerId} onClick={() => ticket.open(side, triggerId)} />
}
