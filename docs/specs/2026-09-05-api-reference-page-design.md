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
- `/docs/api` added to the layout's `PUBLIC` route list so it renders without a session and
  without redirecting to `/login`.
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
- No change to the console's routing guard structure beyond adding one path to the existing
  `PUBLIC` array — no new bypass mechanism, no new auth concept.
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
- Because this route is added to `PUBLIC`, it inherits the layout's existing behavior for public
  pages: it still waits for `session.ready` to resolve once (a `Loading` flash), then renders
  regardless of the outcome — it is never redirected to `/login`, unlike a non-public route. This
  matches how `/login` and `/signup` already behave, and is the smallest change that satisfies
  "reachable without a session" without introducing a second routing-guard code path.

## The two constraints that decide the shape

Both from the issue, both satisfied by this design:

1. **The page cannot live under `/api`.** `/docs/api` is a SvelteKit route, not proxied by
   `API_PREFIXES`, so the console SPA fallback continues to apply there as intended.
2. **It has to be reachable without a session.** Added to `PUBLIC` in `+layout.svelte`, alongside
   `/login`, `/signup`, `/claim`.

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

- `PUBLIC` gains `/docs/api`.
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
