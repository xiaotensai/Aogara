import iconLight from '@/assets/logo/aogara-icon-light.svg'
import iconDark from '@/assets/logo/aogara-icon-dark.svg'
import logoLight from '@/assets/logo/aogara-logo-light.svg'
import logoDark from '@/assets/logo/aogara-logo-dark.svg'
import { cn } from '@/lib/utils'

// The Aogara brand ships two inkings per asset: `-light` for light
// backgrounds (dark glyphs) and `-dark` for dark backgrounds (light glyphs).
// The theme store toggles the `.dark` class on <html>, so we render both and
// let Tailwind's `dark:` variant pick — this tracks light/dark/system without
// subscribing to the store.

/** The square mahjong-tile mark (no wordmark). Portrait ratio ≈ 0.84. */
export function AogaraIcon({ className }: { className?: string }) {
  return (
    <span className={cn('inline-flex shrink-0', className)}>
      <img src={iconLight} alt="" aria-hidden className="h-full w-auto dark:hidden" />
      <img src={iconDark} alt="" aria-hidden className="hidden h-full w-auto dark:block" />
    </span>
  )
}

/** The full logo: tile mark + "Aogara" wordmark. Wide ratio ≈ 2.87. */
export function AogaraWordmark({ className }: { className?: string }) {
  return (
    <span className={cn('inline-flex shrink-0', className)}>
      <img src={logoLight} alt="Aogara" className="h-full w-auto dark:hidden" />
      <img src={logoDark} alt="Aogara" className="hidden h-full w-auto dark:block" />
    </span>
  )
}
