# Header logo design

> **Status:** DRAFT — replace the "dark-factory" wordmark in the console header with the logo mark

## Scope

**In:**

- Adapt `web/static/logo.svg` into an inline Svelte component usable at header scale (a few
  pixels tall in a nav bar, not the ~390×409 illustration size the file ships at).
- Replace the accent-dot + "dark-factory" text in the root header (`web/src/routes/+layout.svelte`)
  with the logo mark.
- Preserve accessible, discoverable text for the brand — the link still needs a name for screen
  readers and for anyone skimming the DOM, per the existing `<a href="/">` semantics.

**Out:**

- No change to the `<title>` suffix (`· dark-factory`) in `web/src/routes/o/[org]/+layout.svelte`
  or any other page's `<title>` — those are document titles, not header UI, and are out of scope
  for this task.
- No change to `web/static/logo.svg` itself (favicon / static asset) — a new inline component is
  added alongside it, the static file is left as the favicon source.
- No change to color theming beyond making the mark themeable (`currentColor`) — no new palette
  tokens are introduced.
- No server-side change of any kind. This is a `web/` presentational change only — no MCP tool, no
  console route, no SQL, no config surface, no migration. Consistent with constraint 2
  (substrate, not workflow): the logo is UI-only and carries no server behavior.

## Assumptions

- The mark reads clearly at a small size (roughly 20–24px tall) next to the org nav links; if it
  did not, the fallback would be to keep it paired with a short label, which is what this design
  does (see below) — so legibility risk is covered either way.
- The wordmark is not dropped entirely: the header keeps a short text label ("dark-factory") next
  to the mark, matching how the rest of the console pairs the accent dot with the name today. Pure
  icon-only branding without any adjacent text is a bigger visual change than "adapt the logo... use
  it instead of the text" calls for, and dropping the text would remove the only header-level
  accessible name if the SVG were ever mis-labeled. Rationale: lower risk, satisfies "use the logo
  instead of the accent dot" literally, keeps brand recognition during the transition.
- `currentColor` fill (rather than the file's hardcoded `#FFFFFF`) is used so the mark inherits the
  header's ink color instead of only rendering on a dark background, since `--color-accent` and
  `--color-ink` vary with the app's light/dark theme.

## Component design

A new component, `web/src/lib/components/Logo.svelte`, renders the mark inline (not via `<img>`,
so it can take `currentColor` and scale crisply without a network request — the file is already a
static asset for the favicon, this is a second, small copy tailored for UI scale). Props: `class`
(pass-through sizing/color classes), default size handled by the consumer via Tailwind (`size-*`).

```svelte
<script lang="ts">
  interface Props {
    class?: string;
  }
  let { class: className = '' }: Props = $props();
</script>

<svg viewBox="0 0 390 409" class={className} fill="currentColor" aria-hidden="true">
  <path d="..." fill-rule="evenodd" />
</svg>
```

`aria-hidden="true"` on the `<svg>` because the enclosing `<a>` carries the accessible name via
adjacent text (see Assumptions) — the icon does not need to repeat it via `role="img"` +
`aria-label`.

## Header change

`web/src/routes/+layout.svelte`, in the header `<a href="/">`:

```svelte
<a href="/" class="flex items-center gap-2 text-sm font-semibold tracking-tight">
  <Logo class="size-5 text-accent" />
  dark-factory
</a>
```

Replaces:

```svelte
<a href="/" class="flex items-center gap-2 text-sm font-semibold tracking-tight">
  <span class="inline-block size-2.5 rounded-sm bg-accent"></span>
  dark-factory
</a>
```

## Testing

- `npm run check` — svelte-check must accept the new component's types.
- `npm run lint` — prettier formatting.
- `npm run build` — confirms the SPA still builds with the new component.
- Manual: `npm run dev`, visually confirm the mark renders at header scale, is legible, and
  inherits the accent color.
- No `npm test` (vitest) coverage exists for header markup today and none is added — vitest in
  this repo exercises the Cloudflare Worker, not component rendering; a snapshot test of static
  header markup would not catch anything a visual check does not already.

## Error Handling & Edge Cases

- None — this is static, presentational markup with no runtime branches, no user input, no
  network call.

## Risks & Open Questions

- None outstanding.
