# API reference page design

> **Status:** DRAFT — render `/api/openapi.json` as a human-readable console page instead of
> linking the footer straight at the raw document

## Premise corrections

None — the issue's description of the current state matches the code: the footer link
(`web/src/routes/+layout.svelte:166`) points at `/api/openapi.json`, which is `openapi::serve`
(`crates/df-web/src/openapi.rs`), an `Auth::Public` handler returning `Json<Value>`.

## Scope

**In:**

- A new SvelteKit route, `/docs/api`, that fetches `/api/openapi.json` at runtime and renders it:
  grouped by tag, each endpoint showing verb, path, summary, description, path parameters, request
  and response schema references, and its `x-dark-factory-auth` level.
- The footer link in `web/src/routes/+layout.svelte` repointed from `/api/openapi.json` to
  `/docs/api`.
- `/docs/api` exempted from the layout's routing guard so it renders without a session, without
  redirecting to `/login`, and — unlike `/login`/`/signup`/`/claim` — without being redirected
  away to `/` for a signed-in visitor either (see Design below; simply adding it to the existing
  `PUBLIC` array is wrong, because that array's second effect is "bounce a signed-in visitor away
  from this page," which would make the footer link unusable once signed in).
- Deep-linking to one endpoint via `#operationId` in the URL hash, working on a hard refresh (not
  only client-side navigation).
- A pure, unit-testable grouping function that the page's rendering is built on top of, so a test
  can assert every endpoint in a document appears in the rendered output with no filtering — see
  Testing.

**Out:**

- `/api/openapi.json` itself does not change shape, response type, or auth level. It remains the
  machine-readable authority; this task only adds a second, human-readable rendering of the same
  document. `web/src/lib/types.ts` continues to be transcribed from it by hand, unaffected.
- No third-party API-doc renderer (Swagger UI / Redoc / Scalar). Per the issue's own reasoning:
  the console's only runtime dependency today is `qrcode`, every page here is `noindex` +
  `no-referrer` and mostly behind a login, and the document is small (currently ~30 endpoints) and
  self-generated — a plain Svelte renderer is both smaller and keeps the dependency surface where
  it is today. This is also a `df-web`/`web` presentational task, not an occasion to add a new
  supply-chain dependency.
- No change to `catalog.rs`, `openapi.rs`'s document shape, or any endpoint's summary/description
  text. Rendering only; the content rendered is exactly what `/api/openapi.json` already returns.
- No localization. Per the issue's own note (referencing #42), the summaries/descriptions are
  English prose written for an API reader; this task explicitly leaves that question to whatever
  resolves #42 and renders the document's strings as-is, in whatever language they are written in.
- No new *auth* concept — the routing guard gains one new list (`UNGATED`, a single-entry array)
  purely to exempt one path from session-based gating; it introduces no new authentication or
  authorization state, just a rendering exemption for a page that talks to no session-gated data.
- No server-side change. `crates/df-web` gains no new route; the existing `/api/openapi.json`
  handler is the only server surface this page talks to. Consistent with constraint 2 (substrate,
  not workflow): this is a console rendering concern, not new server behavior.

Checked against the three constraints in `CLAUDE.md`: this is a `web/`-only rendering change (no
new repo/job/coordination concept — constraint 1 doesn't apply), it adds no new opinion about how
work is specified or planned (constraint 2), and it is not agent-facing at all — it is a page for a
human reading the console's own REST API, so coding-agent agnosticism (constraint 3) is untouched.

## Assumptions

- The chosen path is `/docs/api` (the issue says "or whatever path is chosen"). It reads clearly,
  doesn't collide with `API_PREFIXES` (`/api`, `/oauth`, `/mcp`, `/.well-known`), and doesn't
  collide with any existing top-level route (`login`, `signup`, `claim`, `invite`, `o`, `orgs`,
  `settings`, `trackers`).
- "Grouped by tag" uses the `tag_for()` function's existing tag set (`oauth`, `trackers`, `auth`,
  `me`, `teams`, `repos`, `tokens`, `invites`, `billing`, `orgs`) verbatim — no new tag taxonomy is
  introduced, and the page must not hardcode this list (a tag it has never seen still renders,
  under its own heading, alphabetically ordered with the rest) so a future tag addition in
  `openapi.rs` doesn't need a matching page change.
- "Renders every endpoint" is read literally: the page's rendering must be data-driven from
  whatever `paths` the fetched document contains, with no allowlist/blocklist of paths or methods
  in the frontend. This is what makes a route added to `catalog.rs` show up here automatically,
  matching the issue's framing of this page as "the third reader" of the catalog.
- The page waits for `fetch('/api/openapi.json')` to resolve before rendering the endpoint list
  (an unauthenticated `GET`, same call the raw link makes today) — there is nothing to bake into
  the bundle, matching the console's existing rule for the MCP endpoint and grantable scopes
  (read at runtime from `/.well-known/oauth-protected-resource`).
- Schemas are rendered structurally (property name, type, description, required marker) rather
  than resolving `$ref` recursively into examples — the existing document already carries
  descriptions per property, and a shallow one-level property list is enough for "a reader most
  needs" (verb, path, summary, description, params, request/response shape, auth level) without
  building a general JSON Schema viewer. Nested `$ref`s inside a schema's properties render as
  their type name rather than expanding further.
- "Must render before `session.ready` resolves" (the issue's own wording) is read literally, not as
  "eventually renders once resolved without redirecting." `/login` and `/signup` still show a brief
  `Loading` flash while `session.refresh()` is in flight; `/docs/api` must not — a footer link
  that only works once the session call finishes (or shows a spinner while a slow/failing network
  resolves the session) is exactly what the raw-JSON link avoided by not touching session state at
  all. So `/docs/api` is exempted from the main-content gate itself (the `{#if fatal} … {:else if
  !session.ready} … {:else} children` chain), not merely added to the redirect-skip list — see
  Design.

## The two constraints that decide the shape

Both from the issue, both satisfied by this design:

1. **The page cannot live under `/api`.** `/docs/api` is a SvelteKit route, not proxied by
   `API_PREFIXES`, so the console SPA fallback continues to apply there as intended.
2. **It has to be reachable without a session.** `/docs/api` is exempted from the routing guard's
   redirect logic and the main-content session gate entirely (a new `UNGATED` list, not the
   existing `PUBLIC` array — see Design), so it renders regardless of sign-in state and without
   waiting on `session.ready`.

## Design

### `web/src/lib/openapi.ts` — pure data shaping (new file)

Exports:

- `type OpenApiDocument` — the minimal shape this page reads: `paths` (map of path → map of verb →
  operation object), `components.schemas` (map of name → schema object). Loosely typed (`unknown`
  where the shape isn't relied on) since this mirrors a document generated elsewhere and is not
  the place to fork `df-web`'s OpenAPI vocabulary into a second source of truth.
- `interface EndpointEntry` — `{ method: string; path: string; operationId: string; summary:
string; description: string; auth: string; parameters: ParamEntry[]; requestSchema?: string;
responseSchema?: string }`.
- `interface TagGroup` — `{ tag: string; endpoints: EndpointEntry[] }`.
- `function groupByTag(doc: OpenApiDocument): TagGroup[]` — flattens `doc.paths` into
  `EndpointEntry` values (one per verb present on a path), reads `tags[0]` off each operation as
  the group key, groups, and sorts: groups alphabetically by tag name, endpoints within a group by
  path then by a fixed verb order (`get, post, put, patch, delete`) for stable rendering. This
  function has **no knowledge of which tags exist** — an unseen tag creates its own group. This is
  the function the "renders every endpoint" test (see Testing) exercises directly.
- `function schemaSummary(doc: OpenApiDocument, refName: string): SchemaProperty[]` — resolves
  `components.schemas[refName]`, returns a flat list of `{ name, type, description, required }` for
  its top-level `properties` (or `[]` if the schema isn't an object / isn't found). One level deep,
  per the Assumptions section above.

### `web/src/routes/docs/api/+page.svelte` (new file)

- On mount, `fetch('/api/openapi.json')`, parse as `OpenApiDocument`, call `groupByTag`. Loading /
  error states follow the existing `Loading` / `Alert` component conventions used elsewhere in the
  console (e.g. `web/src/routes/settings/+page.svelte`).
- Renders a heading, then one `<section>` per `TagGroup`, tag name as the section heading.
- Each endpoint renders as a `<article id={operationId}>` (the anchor target for deep-linking)
  showing:
  - A verb badge (`GET`/`POST`/etc.) and the path, monospace.
  - The `x-dark-factory-auth` level, rendered prominently (a small badge, not buried in prose) —
    the issue calls this out explicitly as "the single thing a reader most needs and the thing raw
    JSON buries."
  - Summary (as a subheading) and description (as body text).
  - A parameters table when `parameters` is non-empty: name, description.
  - "Request body" / "Response body" sub-sections when `requestSchema` / `responseSchema` are
    present, each rendering `schemaSummary()`'s property list as a small table (name, type,
    required, description).
- On mount, if `location.hash` is set and matches a rendered `id`, scroll it into view
  (`document.getElementById(hash)?.scrollIntoView()`), after the fetch resolves and the DOM has the
  target element — this is what makes a hard refresh onto `#operationId` work, not only
  client-side navigation (where the browser's own hash-scroll already works because the element
  exists at navigation time coincidentally... on a hard refresh the element doesn't exist until the
  fetch resolves, so the browser's native scroll-on-load has nothing to find yet).

### `web/src/routes/+layout.svelte` changes

Three distinct behaviors are currently conflated in one `PUBLIC` array, and `/docs/api` needs only
one of them — so the array is split rather than overloaded:

- **`PUBLIC`** (existing name, existing purpose, unchanged list: `['/login', '/signup', '/claim']`)
  — paths exempt from "redirect to `/login`" when signed out, *and* redirected to `/` when signed
  in (the auth-flow pages). `/docs/api` is deliberately **not** added here: adding it would make
  the routing effect redirect a signed-in visitor away from the page it just navigated to, which
  is the bug the spec critique caught in the first draft.
- **`UNGATED`** (new, one entry: `['/docs/api']`) — paths that skip the routing-guard `$effect`
  entirely (no redirect either direction, signed in or out) *and* skip the main-content
  `fatal` / `!session.ready` gate, rendering `{@render children()}` unconditionally. This is what
  makes the page "render before `session.ready` resolves": it is not merely fast, it does not wait
  on session state at all, matching the reasoning that the JSON endpoint it mirrors is
  `Auth::Public` and untouched by session machinery.

  ```svelte
  const UNGATED = ['/docs/api'];
  const isUngated = $derived(UNGATED.some((p) => page.url.pathname === p));
  ```

  The routing-guard `$effect` gains an early return: `if (isUngated) return;` before its existing
  body. The main-content template becomes:

  ```svelte
  {#if isUngated}
    {@render children()}
  {:else if fatal}
    ...
  {:else if !session.ready}
    ...
  {:else}
    {@render children()}
  {/if}
  ```

  The header (org nav, sign-out button) still renders around it unchanged — those already read
  `session.signedIn` reactively and degrade to their signed-out appearance on their own; nothing
  about `/docs/api` needs to suppress them.
- The footer link's `href` changes from `/api/openapi.json` to `/docs/api`; link text unchanged
  ("API reference").

## Error Handling & Edge Cases

- `fetch('/api/openapi.json')` failing (network error, non-2xx) renders an `Alert` with a retry
  button, matching the pattern already used in `web/src/routes/settings/+page.svelte`'s `load()`.
- An operation with no `tags` entry (shouldn't happen — `document()` always sets `tags: [tag_for
(path)]` — but the frontend must not crash on it) groups under a literal `"untagged"` group rather
  than throwing.
- An operation with no `parameters` renders no parameters table (not an empty one).
- A path with multiple verbs (e.g. `GET` and `POST` on the same path) produces multiple
  `EndpointEntry` values sharing the same `path` but different `operationId`s and hence different
  anchor ids — no id collision.
- Directly requesting `/docs/api#some-operation-id` for an operation id that does not exist:
  `scrollIntoView` is skipped silently (the lookup returns `null`); page still renders the full
  list.

## Testing

- **Unit test — `web/src/lib/openapi.test.ts` (new, vitest):** the concrete answer to the DoD's "a
  test that the page renders every endpoint the catalog declares, so a route added to `catalog.rs`
  cannot be silently missing from the reference." A vitest test in this repo cannot literally
  invoke `crates/df-web/src/catalog.rs` — the two live in different languages and there is no
  existing cross-language fixture pipeline (`types.ts` is hand-transcribed from the document, not
  generated). Instead, the guarantee is structural: `groupByTag` and the page built on it are
  data-driven with no allowlist or per-path branch, so a test against a synthetic `OpenApiDocument`
  fixture — several tags, a path carrying two verbs, a path with parameters, one with a request
  body — asserts:
  - The number of `EndpointEntry` values returned equals the number of path+verb pairs in the
    fixture (nothing dropped, nothing deduplicated across verbs).
  - Every `(method, path)` pair in the fixture appears exactly once in the flattened output.
  - Each entry's `auth` matches the fixture's `x-dark-factory-auth` value (the field the issue
    calls out as most load-bearing).
  - An unrecognized/novel tag string still produces its own group (proves no hardcoded tag list).

  This proves the renderer cannot silently drop an endpoint regardless of which tags or paths
  `catalog.rs` produces, which is the property the DoD is actually asking for — a stronger
  guarantee than a snapshot against today's specific catalog, which would need updating by hand
  every time a route is added (the exact drift the catalog/document pairing was built to prevent).
- **Component render test — `web/src/routes/docs/api/page.render.test.ts` (new, vitest +
  jsdom):** the pure-function test above proves data isn't lost between the fetched document and
  the grouped structure, but not that the Svelte template itself renders every group/entry it is
  given (a template bug — e.g. an early truncation, a stray `{#if}` — would still pass the pure
  test). This gains one devDependency, `jsdom` (test-only, never bundled), which is the smallest
  addition that lets a component actually mount in vitest; it deliberately does not add
  `@testing-library/svelte` — Svelte 5's own `mount`/`unmount` (from `'svelte'`) are sufficient to
  mount the page component against a stubbed `fetch` returning a synthetic document and read the
  rendered DOM back with plain `document.getElementById`/`querySelectorAll`. The test:
  - Stubs `globalThis.fetch` to resolve the same multi-tag, multi-verb fixture document used by
    the pure-function test.
  - Mounts `+page.svelte` into a detached container, awaits the fetch microtask, and asserts one
    DOM element exists per fixture endpoint's `operationId` (`document.getElementById(id)`) —
    i.e., the anchor targets deep-linking depends on are actually present, not just present in the
    intermediate data structure.
  - Asserts each rendered endpoint element's text content includes its `x-dark-factory-auth` value,
    since that is the field the issue calls out as the one raw JSON buries and the one this page
    exists to surface.
  - This file needs the `jsdom` test environment; add `// @vitest-environment jsdom` at its top
    (vitest's per-file pragma) rather than switching the whole suite's default environment, so
    `worker/index.test.ts` (a Workers/service-worker environment concern, unrelated to a DOM)
    keeps its current default.
- `npm run check` — svelte-check + tsc must accept the new route and lib module.
- `npm run lint` — prettier.
- `npm test` — vitest, includes the new `openapi.test.ts`.
- `npm run build` — confirms the SPA still builds with the new route.
- `cargo test` / `cargo clippy --all-targets -- -D warnings` — no Rust changes are made by this
  task, so these are a no-op check that nothing regressed (vacuously satisfied for the server side).
- Manual: `npm run dev`, visit `/docs/api` signed out and signed in, confirm every group renders,
  confirm the footer link on `/login` points at `/docs/api` and the page renders there too, confirm
  a hard refresh onto `/docs/api#some-real-operation-id` scrolls to that endpoint.

## Risks & Open Questions

- None outstanding. The localization question the issue flags is explicitly deferred to #42 (see
  Scope → Out).
