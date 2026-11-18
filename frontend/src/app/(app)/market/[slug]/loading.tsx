const block = 'rounded-sm bg-surface motion-safe:animate-pulse'

// Mirrors the intended market layout: one column at 360px, market + rail at lg. Heights come
// from the content the skeleton stands in for so the real screen lands without a shift.
export default function MarketLoading() {
  return (
    <main
      aria-busy="true"
      aria-live="polite"
      className="p-xl lg:grid lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)] lg:gap-4xl"
    >
      <span className="sr-only">Loading market</span>

      <div className="flex flex-col gap-xl">
        <div className="flex items-center gap-md">
          <div className={`${block} size-avatar-lg rounded-pill`} />
          <div className="flex flex-1 flex-col gap-sm">
            <div className={`${block} h-heading-md w-3/4`} />
            <div className={`${block} h-label w-1/2`} />
          </div>
        </div>
        <div className={`${block} aspect-video w-full`} />
        <div className={`${block} h-4xl w-full`} />
        <div className="grid gap-xl md:grid-cols-2">
          <div className={`${block} h-4xl w-full`} />
          <div className={`${block} h-4xl w-full`} />
        </div>
      </div>

      <aside className="mt-4xl flex flex-col gap-sm lg:mt-0">
        <div className={`${block} h-heading-sm w-1/2`} />
        <div className={`${block} h-4xl w-full`} />
        <div className={`${block} h-4xl w-full`} />
      </aside>
    </main>
  )
}
