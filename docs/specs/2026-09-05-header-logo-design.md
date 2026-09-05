# Header logo design

> **Status:** IMPLEMENTED — replace the "dark-factory" wordmark in the console header with the logo mark

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

- The task brief is explicit: "use the logo instead of 'dark-factory' in the header" — so the
  visible accent-dot **and** the visible "dark-factory" text are both replaced by the logo mark.
  No visible wordmark remains next to it.
- The mark reads clearly at header scale (roughly 20–24px tall) next to the org nav links — it is
  a solid, simple silhouette (see `web/static/logo.svg`), not fine detail that would blur down.
- Removing the visible text must not remove the link's accessible name. The `<a href="/">` gets an
  explicit `aria-label="dark-factory"` (or an `sr-only` text node) so the brand name is still
  present for screen readers and in the accessibility tree, even though nothing is visibly printed.
  This is what keeps the header a single link with one clear purpose ("go home") while satisfying
  the brief literally.
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

`aria-hidden="true"` on the `<svg>` because the accessible name is supplied by the enclosing `<a>`
(see Header change below) — the icon does not need to repeat it via `role="img"` + `aria-label`.

## Header change

`web/src/routes/+layout.svelte`, in the header `<a href="/">`:

```svelte
<a href="/" class="flex items-center gap-2" aria-label="dark-factory">
  <Logo class="size-6 text-accent" />
</a>
```

Replaces:

```svelte
<a href="/" class="flex items-center gap-2 text-sm font-semibold tracking-tight">
  <span class="inline-block size-2.5 rounded-sm bg-accent"></span>
  dark-factory
</a>
```

No visible text remains next to the mark — `aria-label` on the anchor is the accessible name,
matching the brief's "use the logo instead of 'dark-factory'".

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
