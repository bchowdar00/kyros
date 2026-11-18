# Tiny Next Engineering Ground Rules

This file is the canonical instruction set for every human and agent working in this repository.
`CLAUDE.md` and `.cursor/rules/*` point here; `README.md` explains the why. When a rule here
conflicts with habit, an existing file, or the reference codebases, this file wins. Change it by
PR, never by exception.

## 0. Non-negotiables

Each of these fails CI or review. Waiving one requires a written reason in the PR description.

1. **`main` is always deployable.** `pnpm check`, `pnpm build` and `pnpm e2e` are green on every
   commit. Unfinished work ships behind a flag or stays in the Graphite stack.
2. **Desktop-first, for now.** The product targets `lg` (1024) and above; smaller viewports
   render the mobile-coming-soon screen via a CSS-only branch in the app layout. A UI PR
   without a 1440px screenshot is not reviewable. Mobile-first returns with the mobile app.
3. **Money and probability are integers.** Cents (`*_cents`) and basis points (`*_bps`) end to
   end, exactly as the backend (silk-road) models them. A float never carries money.
4. **Never fabricate data.** No synthetic ticks, no placeholder numbers rendered as real, no
   fixture reachable from a production route. Unknown is shown as unknown.
5. **Fail closed.** A degraded state (stale feed, missing depth, failed fetch) disables the action
   and says why. Never a clamped fallback, never a spinner that means nothing.
6. **Semantic tokens only.** No raw hex/rgb/hsl, no Tailwind default palette, no pixel arbitrary
   values in components. Tokens live in `packages/ui/src/styles/tokens.css`.
7. **Routes are thin and complete.** `app/**` resolves inputs, sets metadata, composes a feature
   screen. Every segment that loads data has `loading.tsx` and `error.tsx`; every segment with a
   param that can miss has `not-found.tsx`.
8. **Accessibility is a merge gate.** Accessible name on every control, keyboard path through
   every flow, visible focus, WCAG 2.2 AA contrast, zero axe violations.
9. **Tests assert behaviour, not implementation.** Query by role and name. Never assert Tailwind
   classes or `data-*` layout attributes.
10. **The bundle stays small.** Nothing ships to the client without a consumer. Route budgets in
    `e2e/routes.ts` fail CI when exceeded.

## 1. Toolchain

- Node 24 (`.nvmrc`), pnpm pinned in `package.json#packageManager`. `pnpm-workspace.yaml` sets
  `engineStrict`; `.npmrc` redirects a stale pnpm 9 to the pinned version. Use `fnm use`.
- Always `pnpm install --frozen-lockfile`. A lockfile diff not caused by a `package.json` change is
  a bug. Never commit it.
- Every version lives in the `catalog:` in `pnpm-workspace.yaml`, pinned exactly. No `^` or `~` in
  package manifests. Install scripts are opt-in through `allowBuilds`.
- Adding a dependency requires a justification in the PR: what it replaces, gzip size, maintenance
  signal, alternatives searched. One library per job. The chosen ones: `@tanstack/react-query`
  (server state), `zustand` (session state), `nuqs` (URL state), `es-toolkit` (utilities),
  `zod/mini` (schemas), `motion` when animation is needed. Lodash, axios, moment, react-icons and
  the full `zod` build are banned by lint.
- `pnpm check` = format + lint (warnings fail, identical to CI) + typecheck + tests + knip +
  generated-file drift. Run it before every push. `pnpm build` before opening a PR. Git hooks
  (lefthook) run the whole gate set on commit — format and lint on staged files, then
  typecheck, tests, knip and the icon drift check — and repeat the whole-graph gates on push.
- Read the installed Next.js docs in `node_modules/next/dist/docs/` before non-trivial
  Next work. The installed docs beat memory.

## 2. Repository layout and dependency direction

```
src, e2e, public    The Next.js app, at the workspace root: thin routes + feature modules. May import packages/*.
packages/ui         Generic primitives, tokens, generated icons. No product knowledge, no fetching.
packages/money      Cents, Bps, rounding, the one formatter. No react, next, DOM or network.
packages/domain     zod/mini wire contracts mirroring silk-road names. No react, next, DOM.
packages/api        fetch → parse → ApiResult. No react, next or DOM globals.
packages/testing    Vitest setup + expectNoA11yViolations, shared by every test suite.
packages/config     tsconfig / postcss presets.
reference/          Read-only reference material. Never imported, never modified.
```

- Dependencies point down: the app imports packages, a package never imports the app, `packages/ui`
  never imports a feature. `money` and `domain` compile without the DOM lib, so a `window` is a
  type error there.
- A feature module `src/features/<feature>/` owns its components, mappers, queries,
  tests. Cross-feature sharing goes through `packages/*`; `src/components` is for the
  app shell only.
- File names are kebab-case and the suffix is the contract:
  `*.fixture.ts` (tests only) · `*.test.ts(x)` · `*.e2e.ts` (Playwright) · `*.server.ts`
  (starts with `import 'server-only'`) · `*.mapper.ts` (wire → view model) · `*.query.ts`
  (TanStack Query hooks and keys).

## 3. Next.js

- Server Components by default. `'use client'` goes on the smallest interactive leaf, never on a
  whole panel because one button holds state. Server data flows down as props.
- Per segment: `page.tsx` (thin), `loading.tsx` (skeleton with the same dimensions as the loaded
  state), `error.tsx`, `not-found.tsx` where a param can miss. `app/global-error.tsx` and
  `app/not-found.tsx` exist.
- Every public route exports `generateMetadata` (title, description, canonical, Open Graph,
  Twitter). `viewport` is exported once at the root with `viewportFit: 'cover'`.
- `typedRoutes` is on. An `href` is a `Route`. Destinations that do not exist yet go through
  `plannedRoute()` in `src/lib/routes.ts` and are listed in `e2e/routes.ts`; delete both entries
  when the route lands.
- Data comes from `src/server/*.server.ts` loaders (wrapped in React `cache`) or
  `*.query.ts` hooks built on `@tiny/api`. A route never imports a fixture; the one loader that
  still does is the prototype's, and its body becomes an `@tiny/api` call.
- Whether a param exists is decided in the segment `layout.tsx`, above the `loading.tsx`
  boundary. A `notFound()` thrown after the shell has streamed answers 200 with `noindex`; from
  the layout it answers a real 404. The e2e suite asserts the status code.
- Every route renders per request (`await connection()` in the root layout) so the CSP nonce from
  `src/proxy.ts` applies. `cacheComponents` stays off until Next supports nonces with a
  prerendered shell. Never read the clock in render for content meant to be cached.
- `src/proxy.ts` is a chain of small single-purpose functions, each with a test.
- `next/image` always has a real `sizes`; `priority` only on the LCP image. `deviceSizes` and
  `imageSizes` in `next.config.ts` list only what the layouts use.
- Fonts load through `next/font/local` in `src/app/fonts.ts`, not hand-written `@font-face`.
- Named exports everywhere; default exports only where Next requires them.

## 4. Responsive (desktop-first interim)

- Interim rule (§0.2): viewports below `lg` render the mobile-coming-soon screen. The
  mobile-specific rules in this section describe the target for when mobile returns; the
  breakpoint system, container queries, touch targets and motion rules apply today.
- Base classes are the mobile layout. `sm:` / `md:` / `lg:` / `xl:` add complexity upward.
  `max-*:` variants are reserved for genuinely mobile-only overrides.
- One breakpoint system, declared in `tokens.css`: `sm` 640, `md` 768, `lg` 1024, `xl` 1280,
  `2xl` 1536. `lg` is the desktop line. No JavaScript breakpoint constants, no device sniffing.
- A component that lives in more than one column width (order book, market card, chart) adapts
  with container queries (`@container`, `@md:`), not viewport breakpoints. Density is a prop, not a
  forked component.
- Layout containers never have `min-w-[Npx]`, fixed `h-[Npx]` on content, or `overflow-hidden` on
  the page. Height comes from content; the viewport is measured in `dvh` / `svh`.
- Touch targets are at least 44×44 CSS px: `min-h-touch min-w-touch`. Scroll regions set
  `overscroll-behavior: contain` and are keyboard reachable; interactive surfaces set
  `touch-action` and a transparent tap highlight.
- Inputs use at least 16px font-size (prevents iOS zoom). No text under 12px except uppercase
  micro-labels (`text-caption`) that are never body copy.
- Fixed bars and sheets pad with `pb-safe-bottom` and friends (`env(safe-area-inset-*)`).
- Panels use one `Sheet` primitive: a side panel at `lg` and above, a bottom sheet below. The
  primary trade action on mobile is a sticky bottom bar.
- When server-side responsive branching is unavoidable, render both branches and hide one with
  CSS. Never `useIsMobile` plus `useEffect`. The only media-query hook is built on
  `useSyncExternalStore` with a server snapshot.
- Every UI PR includes a screenshot at 1440 (and 390 when the coming-soon screen changes).
  Playwright project: Desktop Chrome; gated routes are checked for axe violations and byte
  budgets, and mobile.e2e.ts pins the coming-soon screen at 390.
- Motion: `motion-safe:` only, transitions at most 200ms, nothing decorative. Animate only when
  the animation is the explanation (a price moved).

## 5. Styling and design tokens

- Tailwind CSS v4, CSS-first. Shared theme values are `@theme` tokens in
  `packages/ui/src/styles/tokens.css`. No JavaScript or TypeScript Tailwind config.
- Three layers: raw palette (`:root`, the only place hex is allowed) → `@theme` semantic tokens
  (`--color-*`, `--spacing-*`, `--text-*`, `--radius-*`, `--breakpoint-*`) → components use
  utilities only. Dark and light are a palette swap under `[data-theme]`; the semantic layer never
  changes per theme. Every text token must pass AA against every surface it is used on.
- Arbitrary values are for genuinely one-off layout geometry only. A value that appears twice
  becomes a token. `-[Npx]`, `-[N%]`, `backdrop-blur-[Npx]` and `shadow-[…]` in components fail
  review.
- `backdrop-filter` is expensive on mobile GPUs: at most one blurred surface visible at a time,
  never on rows or table headers.
- If a `--color-*` token exists, use its utility in every colour family. Never
  `text-[var(--color-x)]`.
- Focus is `focus-visible:outline-*` everywhere. One convention; `ring-*` is not used for focus.
- `cn()` is for conditional or merged compositions only, in object form. A static string goes
  straight into `className`. New spacing tokens are also registered in `packages/ui/src/lib/cn.ts`.
- Typography uses the Tiny scale (`text-caption`, `text-label`, `text-body`, `text-heading-sm`,
  `text-heading-md`, `text-display-sm`). Every number that updates renders in the mono cut
  (`font-mono`) and glides via `useTweenedNumber` when it ticks fast. `tabular-nums` is
  banned: the Tiny Roobert proportional cut inverts the feature into narrow proportional
  figures, which is exactly the jitter it was meant to prevent (guard tests enforce this).
- Custom CSS is limited to theme definitions, global resets, and behaviour Tailwind cannot express.

## 6. Components and assets

- Named function declarations. Props extend `ComponentPropsWithRef<'x'>` and are spread onto the
  element. `asChild` via Radix `Slot`. Variants via `cva`. `data-slot` on every compound part.
- Subpath imports only: `@tiny/ui/components/button`, `radix-ui/tabs`, `zod/mini`. No root
  barrels.
- React Compiler is on. No `useMemo`, `useCallback` or `memo` without a measurement. `'use no
  memo'` only with a TODO and a ticket.
- Every `@tiny/ui` component ships with a behaviour test that calls `expectNoA11yViolations` and
  an accessible-name contract in its props type (`IconButton` requires `aria-label`).
- Icons are generated from `packages/ui/src/assets/icons/<size>/` by `pnpm icons:generate`.
  `currentColor`, `aria-hidden` by default. Never hand-author SVG paths for supplied assets.
- Visual assets are committed locally, never referenced from temporary Figma URLs.
- Charts render on canvas. Never inline an SVG path with more points than twice its CSS pixel
  width; downsample first. No SVG `filter` on data paths.
- Any list that can grow is virtualized.

## 7. Data, state and realtime

State has exactly one home.

| Kind | Home | Never |
| --- | --- | --- |
| Server data | TanStack Query, keys from one factory, `staleTime` per data class | a global store |
| Streaming (book, trades, fills) | realtime store outside React via `useSyncExternalStore` | the query cache |
| Trade ticket | one pure state-machine instance per market, bound by one hook | React context |
| Shareable UI (filter, tab, sort) | URL search params via `nuqs` | a store |
| Ephemeral UI (open, hover, focus) | `useState` | lifted |
| Session (theme, locale, wallet) | one `zustand` store | a pyramid of providers |

- A provider, store or SDK is mounted with its first consumer, never before. Until then the
  dependency is pinned and listed in `knip.ts` with that reason.
- `@tiny/api` is the only HTTP client: request → parse (Standard Schema, `zod/mini`) → select,
  returning `ApiResult`. It never throws into UI code. Retries live there and nowhere else, with a
  wall-clock budget; a POST retries only when it carries an idempotency key. Every mutation that
  moves money carries one.
- Provenance naming: raw types mirror the wire field names verbatim (`price_cents`,
  `probability_bps`, `occurs_at`). Renaming happens only in a `*.mapper.ts` that produces the view
  model.
- Realtime: one multiplexed socket owned by a connection manager outside React; ref-counted
  subscriptions; exponential backoff with jitter, retrying forever; a sequence gap triggers a
  re-snapshot and marks the data `stale`. Every live number carries visible freshness
  (`live`, `stale`, `disconnected`). A hidden tab pauses non-session sockets.
- `useEffect` is for real side effects only: DOM listeners, observers, imperative widgets. Never
  for fetching, deriving state, syncing props, or notifying a parent.
- Optimistic updates clear when the server confirms with the client id, never on a timer.
- Logout purges caches deny-by-default. A persisted query needs an explicit allowlist entry.

## 8. Money, numbers and copy

- `@tiny/money` owns arithmetic and formatting with branded `Cents` and `Bps`. Integer minor
  units travel as integers; `applyBps` rounds half-to-even on `bigint`; division happens once, in
  `createMoneyFormatter`. A `new Intl.NumberFormat` anywhere else fails review. One formatter per
  locale, not per render.
- Copy is inline English for now; a message catalog arrives with the second locale. Keep strings
  in the component that owns them, never in fixtures. Dates via `Intl.DateTimeFormat`, relative
  time via `Intl.RelativeTimeFormat`.
- Colour is never the only carrier of up/down or yes/no. Pair it with a sign, an icon, or text.

## 9. Performance budgets

Enforced in `e2e/budgets.e2e.ts` against the production build on every PR, as encoded
bytes on the wire. Budgets per gated route live in `e2e/routes.ts`.

- Any public route: first-load JS at most 170 KB gzip, HTML document at most 100 KB gzip. New
  routes start with those numbers; tighten, never loosen. A route joins `gatedRoutes` when it is
  built mobile-first; the prototype market screen is not there yet.
- Field targets at p75 on a mid-tier Android over 4G: LCP under 2.0s, INP under 200ms, CLS under
  0.05.
- No blocking third-party scripts. Analytics load after interactive.
- Skeletons match final layout dimensions. Tab containers reserve their minimum height.
- Heavy client-only modules (chart, wallet SDKs) load via `next/dynamic` with `ssr: false` and a
  skeleton.
- Measure before optimising. Run `pnpm analyze` before adding any dependency over 20 KB gzip.

## 10. Accessibility

- Semantic HTML first: `button`, `a`, `table`, `dl`, landmarks with names.
- An accessible name on every interactive element; icon-only buttons require `aria-label` by
  type. Visually hidden text (`sr-only`) beats `aria-label` on a non-interactive element.
- Keyboard-complete flows, visible focus, Escape closes, focus returns to the trigger. Scroll
  regions are focusable. Roving tabindex comes from Radix.
- Live prices use `aria-live="polite"` with throttling. Never flood a screen reader.
- `prefers-reduced-motion` and `prefers-color-scheme` are respected.
- Contrast AA: 4.5:1 for text, 3:1 for UI. Tokens must pass in every theme.
- `expectNoA11yViolations` (axe-core) runs in component tests; `@axe-core/playwright` runs on every
  gated route in every device project. Zero violations.

## 11. Security and privacy

- `src/env.ts` is the only reader of `process.env` (oxlint `node/no-process-env`),
  validated with `@t3-oss/env-nextjs` and `zod/mini`. Server-only variables go behind
  `import 'server-only'`.
- No secrets in the repo, in `NEXT_PUBLIC_*`, or in a client bundle. `.env*` is gitignored;
  `.env.example` documents every variable.
- Content Security Policy is nonce-based and strict, built in `src/lib/security/csp.ts` and set
  per request in `src/proxy.ts`: `script-src 'self' 'nonce-…' 'strict-dynamic'`, no
  `unsafe-eval` outside development, `frame-ancestors 'self'`, `object-src 'none'`. `style-src`
  keeps `'unsafe-inline'` for server-rendered style attributes; never add `unsafe-inline` to
  scripts. Static headers (HSTS, `nosniff`, referrer, permissions) live in `next.config.ts`;
  `poweredByHeader` is off. The e2e suite asserts all of it.
- Sessions live in httpOnly, Secure, SameSite cookies. Never a token in web storage.
- `dangerouslySetInnerHTML` only for escaped JSON-LD. User-generated links: parse the host, check
  an allowlist, `rel="noopener noreferrer nofollow ugc"`.
- Untrusted strings are sliced to 2,000 characters before any regex. No regex literal in render.
- No `console.*` in production code (lint). Never log keys, signatures, tokens, addresses, or
  emails. Error boundaries never render the error message.
- Contract addresses and money constants require two approvals via CODEOWNERS.

## 12. Observability

Not wired yet, decided: error reporting in `instrumentation.ts` / `instrumentation-client.ts`
with every `error.tsx` reporting, Web Vitals via `useReportWebVitals`, and one typed analytics
event bus with PII redaction by type. All three land before the first production deploy, each
with its vendor.

## 13. Testing

| Layer | Tool | Gate |
| --- | --- | --- |
| Pure packages (money, domain, api) | Vitest + fast-check property tests, MSW for HTTP | every export; invariants as properties |
| Components | Vitest + Testing Library + `expectNoA11yViolations` | every interactive component |
| End to end | Playwright in `e2e`, Desktop Chrome | smoke on every route; axe, headers, budgets, 404 status on gated routes; coming-soon at 390; on every PR |

- Query by role, name, or label. `data-testid` only when no accessible query exists, never for
  layout assertions. `toHaveClass` is banned for new tests; the three prototype tests that use it
  go with the prototype.
- Network is mocked with MSW, not module mocks. No markup snapshots.
- Fixtures are typed, deterministic, realistically sized, and never imported by production code.

## 14. Code authoring

- Reuse before writing. Search `packages/*` and `features/*` first; duplicated logic is a bug.
  Extend, do not fork.
- No premature abstraction: a hook with one caller, a wrapper that adds nothing, a `utils.ts`.
  Extract for a name, not for hypothetical reuse. The same applies to infrastructure: nothing
  lands before its first consumer.
- Names: no `data`, `result`, `temp`, `info`, `item`, `obj` at function scope; no `*Helper`,
  `*Manager`, `*Util`; no generic verbs. Magic numbers become named constants with the reason.
- Trust the types. No `?.` on a non-optional value, no `?? []` on a required one, no `try/catch`
  around code that cannot throw. Validate at trust boundaries only.
- TypeScript stays at `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`,
  `noPropertyAccessFromIndexSignature`, `noImplicitReturns`, `erasableSyntaxOnly`. No `as any`
  or `as unknown as` without a justifying comment. No non-null assertions (lint).
- Formatting is Biome's: single quotes, no semicolons, width 100. The commit hook applies it.
- No spread inside `reduce`; use `toSorted`, a `Map`, or a mutable accumulator. No nested
  ternaries deeper than two.
- An empty `catch` is banned (lint). Catch to recover or to translate; otherwise let it reach a
  boundary.
- Comments explain why. Delete commented-out code. No file banners.
- Files under 300 lines, components under 150. Split by responsibility.

## 15. Git and delivery

- Graphite stacks. One planned step per PR, at most 400 changed lines, green `pnpm check`,
  `pnpm build` and `pnpm e2e`. Do not include the next stack step's work in the current PR.
- The PR body follows `.github/pull_request_template.md`; delete the lines that do not apply.
  Design QA evidence lives in the PR, not in files committed to the repo.
- CODEOWNERS per package. Two approvals for `packages/money`, `packages/domain`, contract and
  environment files.
- Conventional commits (`feat(web): …`). Rebase within a stack; no merge commits.
- Anything not ready for users ships behind a feature flag. No dead routes on `main`.

## 16. Generated and derived files

Never hand-edit; regenerate and commit or ignore as listed.

- `next-env.d.ts`: regenerated by `next dev|build|typegen`; gitignored.
- The `nextjs-agent-rules` block at the end of this file, and `CLAUDE.md`: written by `next dev`;
  committed.
- `packages/ui/src/icons/**`: `pnpm icons:generate`; committed; drift fails CI.
- `pnpm-lock.yaml`: only changes with a `package.json` change.
- `.next/types/**`: `next typegen`; gitignored; provides `PageProps`, `LayoutProps`, `Route`.

## 17. Deliberately open

Chart library (canvas, in progress on its own branch), the realtime wire protocol, auth and
custody, the error-reporting vendor, Storybook, i18n. Decide each by a spike and record it as an
ADR in `docs/adr/`.

<!-- BEGIN:nextjs-agent-rules -->

# This is NOT the Next.js you know

This version has breaking changes — APIs, conventions, and file structure may all differ from your training data. Read the relevant guide in `node_modules/next/dist/docs/` (resolved from this file's directory; in monorepos the `next` package may not be visible from the repo root) before writing any code. Heed deprecation notices.

This block is written and re-added by `next dev` — verify at `node_modules/next/dist/server/lib/generate-agent-files.js`. Removing it from a diff only re-creates the uncommitted change; committing it with your work keeps the tree clean.

<!-- END:nextjs-agent-rules -->
