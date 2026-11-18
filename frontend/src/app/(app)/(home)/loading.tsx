const block = 'rounded-sm bg-surface motion-safe:animate-pulse'

export default function HomeLoading() {
  return (
    <main
      aria-busy="true"
      aria-live="polite"
      className="mx-auto w-full max-w-[90rem] px-4xl py-4xl"
    >
      <span className="sr-only">Loading markets</span>
      <div className="flex flex-wrap gap-xl">
        <div className={`${block} h-market-card-chart w-full max-w-market-card`} />
      </div>
    </main>
  )
}
