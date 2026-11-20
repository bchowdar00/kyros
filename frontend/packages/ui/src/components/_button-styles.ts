import { cva, type VariantProps } from 'class-variance-authority'

const buttonBaseClasses =
  'inline-flex shrink-0 cursor-pointer items-center justify-center gap-sm rounded-sm border-[1.5px] font-semibold transition-[color,background-color,border-color,box-shadow,transform] duration-150 ease-out-soft active:scale-[0.98] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring disabled:pointer-events-none disabled:border-border-subtle disabled:bg-surface disabled:text-muted-foreground'

const variants = {
  variant: {
    primary:
      'border-action bg-action text-on-action shadow-glow-action hover:border-foreground hover:bg-foreground hover:text-background',
    secondary:
      'border-foreground bg-transparent text-foreground hover:bg-foreground hover:text-background',
    danger:
      'border-foreground bg-foreground text-background hover:border-action hover:bg-action hover:text-on-action',
    ghost:
      'border-transparent bg-transparent text-muted-foreground hover:bg-surface hover:text-foreground',
  },
} as const

export const buttonVariants = cva(buttonBaseClasses, {
  variants: {
    ...variants,
    size: {
      small: 'h-2xl px-md text-label',
      medium: 'h-3xl px-lg text-label',
      large: 'h-4xl px-xl text-body',
    },
  },
  defaultVariants: {
    size: 'medium',
    variant: 'primary',
  },
})

export const iconButtonVariants = cva(buttonBaseClasses, {
  variants: {
    ...variants,
    size: {
      small: 'size-2xl text-label',
      medium: 'size-3xl text-label',
      large: 'size-4xl text-body',
    },
  },
  defaultVariants: {
    size: 'medium',
    variant: 'ghost',
  },
})

type ButtonVariantProps = VariantProps<typeof buttonVariants>

export type ButtonSize = NonNullable<ButtonVariantProps['size']>
export type ButtonVariant = NonNullable<ButtonVariantProps['variant']>
