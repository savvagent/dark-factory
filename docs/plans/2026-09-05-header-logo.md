# Header logo — swap the wordmark for the mark

## Goal

Replace the accent-dot + "dark-factory" text in the console's root header with an inline,
theme-aware rendering of the logo mark, preserving an accessible name on the header link.

## Status — 2026-09-05

Not started. One task.

**Spec:** `docs/specs/2026-09-05-header-logo-design.md` — read it first. This plan implements it
exactly.

## Global Constraints

- `web/` conventions: Svelte 5 runes only (`$state`/`$derived`/`$props`/`$effect`), no `export let`,
  no Svelte 4 stores, Tailwind v4, `adapter-static` (no SvelteKit server).
- No AI self-attribution anywhere (commits, comments, docs).
- Run `npm run lint` (prettier) before committing any `web/` change; there is no `cargo fmt`
  equivalent needed since this task touches no Rust code.
- This change touches only `web/` presentational files — no SQL, no MCP tool, no console API route,
  no migration, no config surface. Tenant isolation, metering, and public-interface rules do not
  apply; no cross-org test and no `df-billing::classify` step are needed.
- Gates: `npm run check` (svelte-check + tsc), `npm run lint` (prettier), `npm run build`. `npm test`
  (vitest) exercises the Cloudflare Worker only and has no bearing on this change — running it is a
  vacuous pass, not a skip.

## File Structure

| File                                          | Responsibility                                                             |
| ---------------------------------------------- | --------------------------------------------------------------------------------- |
| `web/src/lib/components/Logo.svelte`           | **Create.** Inline SVG mark component, `currentColor` fill, pass-through `class`. |
| `web/src/routes/+layout.svelte`                | **Modify.** Header `<a href="/">`: swap accent-dot + text for `<Logo>` + `aria-label`. |

## Task Order & Rationale

Single task — the component and its one call site are tightly coupled and small enough to land
together; there is no intermediate state worth a checkpoint between them.

## Task 1 — Add `Logo` component and use it in the header

**Files:** `web/src/lib/components/Logo.svelte` (new), `web/src/routes/+layout.svelte` (modify)
**Interfaces:** `Logo` consumes a `class` prop (pass-through sizing/color); produces an inline
`<svg>` painted with `currentColor`. No other component or route consumes `Logo` yet.

- [ ] Read `web/static/logo.svg` (`viewBox="0 0 390 409"`, single `<path>`, currently
      `fill="#FFFFFF"`) and confirm the path data to copy verbatim into the new component.
- [ ] Create `web/src/lib/components/Logo.svelte`:
  ```svelte
  <script lang="ts">
    interface Props {
      class?: string;
    }
    let { class: className = '' }: Props = $props();
  </script>

  <svg viewBox="0 0 390 409" class={className} fill="currentColor" aria-hidden="true">
    <path
      d="<paste the exact path data from web/static/logo.svg>"
      fill-rule="evenodd"
    />
  </svg>
  ```
- [ ] In `web/src/routes/+layout.svelte`, add `import Logo from '$lib/components/Logo.svelte';` to
      the script block's import list (alongside the existing `Alert`/`Loading` imports).
- [ ] Replace the header brand `<a>`:
  ```svelte
  <a href="/" class="flex items-center gap-2 text-sm font-semibold tracking-tight">
    <span class="inline-block size-2.5 rounded-sm bg-accent"></span>
    dark-factory
  </a>
  ```
  with:
  ```svelte
  <a href="/" class="flex items-center gap-2" aria-label="dark-factory">
    <Logo class="size-6 text-accent" />
  </a>
  ```
- [ ] Run `cd web && npm run check` — svelte-check must pass with no new errors.
- [ ] Run `npm run lint` — prettier must report no formatting issues (run `npm run lint -- --write`
      first if it does, then re-check).
- [ ] Run `npm run build` — confirms the static bundle still builds.
- [ ] Manual visual check: `npm run dev`, load the console (any page under `/`), confirm the header
      shows the logo mark (no visible "dark-factory" text or accent dot), it is legible at header
      scale, and it inherits the accent color. Confirm the link still navigates to `/` and that a
      screen reader / the accessibility tree reports the link's name as "dark-factory" (e.g. via
      browser devtools' Accessibility panel on the `<a>`).
- [ ] Format and commit: no Rust changes, so no `cargo fmt`; run `npm run lint` once more as the
      formatting gate, then `git add -A && git commit -m "web: swap header wordmark for logo mark"`.
