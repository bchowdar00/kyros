'use client'

import { Button } from '@kyros/ui/components/button'
import Image from 'next/image'
import { useEffect, useRef } from 'react'

export function WidgetBackButton({ onClose }: { onClose: () => void }) {
  const button = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    button.current?.focus({ preventScroll: true })
  }, [])

  return (
    <Button
      aria-label="Back to strategy composer"
      ref={button}
      onClick={onClose}
      variant="ghost"
      className="relative w-fit gap-0 rounded-sm border-highlight py-0 pr-sm pl-0 text-muted-foreground after:absolute after:inset-x-0 after:-inset-y-sm after:content-['']"
    >
      <Image alt="" src="/assets/market/trade-back.svg" width={24} height={24} />
      <span>esc</span>
    </Button>
  )
}
