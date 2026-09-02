# `web/` — the dark-factory console

SvelteKit 2 · Svelte 5 (runes) · Tailwind v4 · TypeScript, strict.

Everything a human touches: signing up, enrolling an authenticator, members, teams, repos
and their live leases, a read-only queue, the usage meter, and the page that tells any MCP
client how to connect.

```bash
npm install
npm run dev      # Vite on :5173, proxying /api /oauth /.well-known to DF_API_ORIGIN
npm run check    # svelte-check + tsc over worker/ — the type gate
npm run lint     # prettier --check
npm run build    # static bundle in build/
npm run deploy   # build, then wrangler deploy — see docs/deploy/cloudflare.md
```

`npm run dev` needs a server behind it. Set `DF_API_ORIGIN` if it is not on
`http://127.0.0.1:8080`. **Until task 13 binds a port there is nothing to proxy to**, so
the dev server renders the shell and every request 502s.

## Why it is a single-page app

Not a performance choice. The console's session is an `HttpOnly`, `__Host-`-prefixed
cookie, and `__Host-` means the browser refuses to store it unless it is `Secure`, has
`Path=/`, and carries **no `Domain`** — so the cookie is bound to one origin and cannot be
sent anywhere else.

A SvelteKit server rendering these pages would therefore have to hold that credential to
fetch on the user's behalf: a second process with the keys to every console session, for
pages that are behind a login and cannot be cached anyway. Building to static files that
`df-server` serves beside `/api` keeps the cookie in exactly one place — the browser — and
makes CORS a non-question, because there is no second origin.

The same fact drives `vite.config.ts`. Dev proxies `/api`, `/oauth`, and `/.well-known`
rather than pointing `fetch` at another port, because a cross-port request would not carry
the session and no CORS header could rescue it.

## What holds across the app

**No credential is spent on a `GET`.** Verification, recovery, and invitation mails point
at pages here — `/verify`, `/recover`, `/invite/{org}` — which render a button that
`POST`s the token. Mail scanners and link-preview fetchers follow every URL in every
message, and a single-use `GET` is burned before the human clicks it. `df-web`'s
`every_single_use_redemption_is_a_post` and `the_emailed_urls_are_pages_not_endpoints`
assert the server's half of the same bargain.

**An org you are not in renders as "no such organization".** The API answers `404` for both
a nonexistent org and one the caller is not in, precisely so the two cannot be told apart.
A console that helpfully said "you don't have access to acme" would undo that from the
client side.

**Roles decide what is _shown_, never what is _allowed_.** `OrgContext.isAdmin` hides
buttons that would fail. Every one of them is still refused by the server, on every
request, by `OrgCtx`.

**The queue is read-only.** There is no button here that changes a job. A job is created
and finished by the agent doing the work, over MCP; a human pressing "mark complete" would
be telling the queue something they cannot observe, and the audit trail would record it as
fact.

**Nothing about the deployment is baked into the bundle.** The MCP endpoint and the
grantable scopes come from `/.well-known/oauth-protected-resource` at runtime. A hard-coded
MCP URL is how a staging or self-hosted deployment ends up printing a connect command that
points at production.

## Layout

| Path                        | What it is                                                                   |
| --------------------------- | ---------------------------------------------------------------------------- |
| `src/lib/api.ts`            | The only place that talks to `df-web`. `ApiError` carries the stable `code`. |
| `src/lib/types.ts`          | The wire types, transcribed from `df-web`'s OpenAPI document.                |
| `src/lib/session.svelte.ts` | Who is signed in. A rune module, not a store.                                |
| `src/lib/org.svelte.ts`     | The org the current route is about, via context.                             |
| `src/lib/clients.ts`        | One recipe per coding agent, all the same shape.                             |
| `src/routes/`               | Public pages at the root; org pages under `/o/[org]`.                        |
| `worker/index.ts`           | The Cloudflare Worker: serves this bundle, proxies the API to `df-server`.   |
| `wrangler.jsonc`            | That Worker's config. `worker/tsconfig.json` type-checks it separately.      |

Org pages live under `/o/[org]` rather than `/[org]` so that no org slug can ever collide
with a page name. The routes the _server_ names — `/login`, `/verify`, `/recover`,
`/invite/{org}`, `/settings/billing` — are fixed by what goes in email and in
`df-billing`'s upgrade prompt, and must not be renamed here alone.

## Deploying to Cloudflare

`npm run deploy` uploads `build/` and `worker/index.ts` as one Worker: Cloudflare serves the
SPA from its own network and the Worker forwards `/api`, `/oauth`, `/.well-known`, `/mcp`,
`/healthz` and `/readyz` to `df-server`.

It proxies rather than redirecting for the same reason the app is a SPA at all — the
`__Host-` cookie is bound to one origin, so the API cannot live on a second hostname. The
prefix list in `worker/index.ts` mirrors `API_PREFIXES` in `crates/df-server/src/lib.rs` and
must not drift from it.

Two things on the `df-server` side are not optional and are easy to miss:
`DF_ALLOWED_HOSTS` must name the origin's own hostname, or every authenticated MCP call
fails while everything else looks healthy; and the origin must refuse traffic that did not
come through Cloudflare, because `DF_CLIENT_IP_HEADER=cf-connecting-ip` is only trustworthy
while Cloudflare is the one writing it. [`docs/deploy/cloudflare.md`](../docs/deploy/cloudflare.md)
has both, and what a local `wrangler dev` run proved about each.

None of this is required to run the console. `df-server` still serves `build/` itself, which
is what a self-hosted deployment does.

## Adding a client to the connect page

Add an entry to `CLIENTS` in `src/lib/clients.ts`. Every client gets the same two forms —
OAuth and access token — because dark-factory is coding-agent agnostic by constraint, and a
console that gave one agent a bespoke wizard and the rest a footnote would be the first
place that promise quietly broke.
