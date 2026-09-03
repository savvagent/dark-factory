# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

**dark-factory** is a hosted, multi-tenant MCP server for coordinating agentic coding work
across enterprises and teams. Server-only — no TUI, no PTY, no local binary, no plugin.

Read [`docs/specs/2026-09-01-dark-factory-design.md`](docs/specs/2026-09-01-dark-factory-design.md)
before any non-trivial change. The build order is in
[`docs/plans/2026-09-01-milestone-1.md`](docs/plans/2026-09-01-milestone-1.md).

## The three constraints

These decide scope disputes. When a change conflicts with one of them, the change is wrong.

1. **Coordination is anchored on repos.** A repo is a first-class entity, not a config
   string. Jobs belong to repos (`repo_id` is `NOT NULL`); leases are repo-scoped; an
   unresolvable repo is an error naming the registered slugs, never a silent fallback.
2. **Substrate, not workflow.** dark-factory ships no opinion about how work is specified,
   planned, reviewed, or measured. If a capability could live either in the server or in a
   customer's own skill calling the server, **it belongs in the skill**. Jobs carry an
   opaque `metadata` JSONB field for exactly this reason — the server never interprets it.
3. **Coding-agent agnostic.** Claude Code, Copilot CLI, Cursor, Codex and anything else
   speaking MCP are equally first-class. Never depend on a specific client's hook, plugin,
   or skill system; never validate `agentType` against a list; never add a client-specific
   tool annotation. If a feature only works in one agent, it does not ship.

## Tenant isolation — the rule that outranks convenience

One org must never see or touch another's data. Two independent guards, and **both** are
required for any new tenant table:

1. **API shape.** Tenant data is reachable only through `Tx`, which cannot be constructed
   without an `OrgId`. Every statement carries `org_id = $1` explicitly, even though RLS
   would also filter it — the predicate keeps plans index-friendly and intent legible.
2. **Row-level security.** `Db::begin` issues `SET LOCAL ROLE df_app` **and**
   `SET LOCAL app.org_id`. Both matter. Postgres exempts superusers and table owners from
   their own RLS policies, and the connecting user is frequently one or both, so **without
   the `SET LOCAL ROLE` the policies do nothing at all**. This was verified empirically,
   not assumed.

   The role is issued *only when it can be assumed*, because `CREATE ROLE` needs a
   cluster-level privilege that managed Postgres does not hand out — on Fly's managed
   cluster `df_app` cannot be created at all. There, every tenant table being
   `FORCE ROW LEVEL SECURITY` carries the same guarantee: FORCE applies the policies to
   the table's owner, and the connecting role is neither a superuser nor `BYPASSRLS`.
   **Nothing assumes which of the two shapes it is in.** `Db::verify_tenant_isolation`
   reads it back out of the catalog as the role a tenant transaction actually runs as,
   and `df-server` refuses to bind a port unless one of them holds. Guard 2 is the one
   guard the *environment* can switch off, so it is the one guard that gets checked at
   startup rather than trusted.

When you add a tenant table: give it a `NOT NULL org_id`, add it to the `tenant_tables`
array in `0007_rls.sql`, and add a cross-org negative test. A tenant-scoped function
without a cross-org negative test is not done. The policy must be named
`<table>_tenant_isolation` — `verify_tenant_isolation` discovers tenant tables by that
convention rather than from a list it would have to be told about, so a differently named
policy is a table the startup check will not vouch for.

**Testing WebAuthn needs a real authenticator.** `webauthn-authenticator-rs`'s software
tokens do **not** support resident keys (`SoftToken` says "These will be supported in
future"), so `tests/passkeys.rs` softens the challenge it hands the fake authenticator and
names the credential in `allowCredentials`. Only what the fake sees is softened; every
server step is the production one. What that cannot cover — a browser finding a credential
unprompted — needs a CDP virtual authenticator, which is how the flow was actually verified.

**A privilege granted to `df_app` is not a protection.** `df_app` does not exist on
managed Postgres, so `REVOKE … FROM df_app` protects nothing there. Express the rule as a
policy instead: `audit_events` is append-only because it has no `UPDATE` policy, which
under FORCE binds the table's owner too — strictly stronger than the grant it replaced,
and it survives both deployment shapes. `#[sqlx::test]` connects as a superuser and
bypasses RLS, so a test of such a policy **must** `SET LOCAL ROLE df_app` explicitly or it
passes against no policy at all.

Note that ordinary cross-org tests pass on the strength of guard 1 alone. The tests that
actually exercise RLS are the `rls_scopes_*` ones in `tests/isolation.rs`, which issue
deliberately **unscoped** SQL inside a pinned transaction. Keep that distinction — a test
suite that cannot tell the two guards apart cannot tell you when one has broken.

## Commands

```bash
podman compose up -d          # Postgres 16 on host port 15433
cp .env.example .env          # DATABASE_URL for sqlx
cargo test                    # everything
cargo test -p df-core --test isolation   # tenant isolation
cargo test -p df-core --test queue       # queue behaviour
cargo clippy --all-targets -- -D warnings
cargo fmt --all

cd web && npm install
npm run check                 # svelte-check + tsc over worker/ — the console's `cargo test`
npm run lint                  # prettier --check — the console's `cargo fmt --check`
npm test                      # vitest — the Cloudflare Worker's routing rule
npm run build                 # static bundle into web/build

cargo run -p df-server        # everything on one port, reading .env
podman build -t dark-factory .   # console stage + rust stage + slim runtime
```

`DF_PUBLIC_URL` and `DF_ENCRYPTION_KEY` are required with no defaults; `.env.example` says
why for each. Build `web/` first or every console page answers `404` while the API works.

Integration tests are `#[sqlx::test]` against a real Postgres — one fresh throwaway
database per test, migrations auto-applied. There are no mocks for the database, on
purpose: RLS, `FOR UPDATE`, `LISTEN`/`NOTIFY`, and enum round-tripping are the things most
likely to be wrong, and a mock cannot tell you about any of them.

## Architecture

One binary (`df-server`) mounts every HTTP surface on one port. The crates are a
compile-time layering discipline, not separate services.

| Crate | Responsibility |
|---|---|
| `df-core` | Domain + **all** SQL. No HTTP, no auth. Every tenant fn takes an `OrgId`. |
| `df-auth` | OAuth 2.1 AS, TOTP, enterprise OIDC federation, personal access tokens. |
| `df-mcp` | `rmcp` Streamable HTTP server, tool surface, resource-server middleware. |
| `df-billing` | Usage recording, period counters, tier limits. |
| `df-trackers` | GitHub App + JIRA clients, webhook ingest, two-way sync. |
| `df-web` | Console REST API, session cookies, the AS's HTML endpoints. |
| `df-server` | Config, migrations, router assembly, graceful shutdown. |

`web/` is the console UI — SvelteKit 2 / Svelte 5 runes / Tailwind v4, TypeScript strict —
built to static files that `df-server` serves beside `/api`. See `web/README.md`.

**Every SQL statement lives in `df-core`.** A query in `df-mcp` or `df-web` is a bug — it
bypasses the `Tx` pinning that guard 2 depends on.

## The MCP surface

`df-mcp` is the only crate a customer's agent talks to, and four conventions hold across
every tool in it:

- **The caller comes from the request, not the session.** An MCP session spans many HTTP
  requests; `require_bearer` introspects the token on each one and puts the `Principal` in
  the request extensions, which the transport carries into the handler as
  `Extension<http::request::Parts>`. This is what makes revocation take effect on the next
  call instead of at some unbounded later point — do not cache a principal on the service.
- **The org comes from the token and nowhere else.** No tool takes an org argument.
- **Every result is a one-field object** — `{"job": …}`, `{"jobs": […]}` — defined in
  `tools::out`. MCP requires `outputSchema` to be rooted at `object`, so a bare array is
  not a legal result, and the envelope can gain a field without breaking callers.
- **Descriptions are the documentation.** The reader is an LLM that has never seen these
  docs: say what the tool does, when to reach for it instead of a neighbour, and what the
  failure means. `tests/tools.rs` asserts the tool list and that every tool describes
  itself, so a tool that silently disappears fails a test rather than a customer.

## The console surface

`df-web` serves everything a human touches, plus the authorization server's HTTP endpoints
— `/oauth/authorize` is a browser surface that needs the console's session cookie, which is
why it lives here and not in `df-mcp`. Four conventions:

- **Authorization is an extractor, not a handler's first line.** `OrgCtx` resolves the
  caller, the `{org}` path segment, and their role before any handler body runs;
  `require_admin()` / `require_owner()` narrow it. A handler that forgets is a handler that
  serves another tenant's data, and a type catches that where a review checklist does not.
- **An org you are not in is `404`, never `403`.** A `403` on a real slug and a `404` on a
  fake one turns any signed-in account into a directory of who uses the product.
- **The router and the OpenAPI document are built from one list.** Adding a route means
  adding it to `catalog.rs` with its summary and description; `router()` mounts the list and
  `openapi::document` renders it. A route not in the catalog is not reachable, on purpose.
- **No credential is ever spent on a `GET`.** Link-preview fetchers follow every URL in
  every message, so an invitation link points at a console *page* that renders a button, and
  the button `POST`s the token. The product sends no mail, which narrows the list but does
  not retire the rule — a code pasted into Slack gets unfurled just as eagerly. `every_single_use_redemption_is_a_post` asserts
  it. The session cookie's attributes — `HttpOnly`, `Secure`, `Path=/`, `SameSite=Lax`, and
  the `__Host-` prefix — are asserted for the same reason: losing one is a silent
  regression that no other test would notice. `Lax` specifically, because `Strict` would
  drop the cookie on the top-level navigation into `/oauth/authorize`.

**There is no mailer, and adding one is a product decision, not a convenience.** Nothing in
this product sends email: an authenticator app is the only factor, recovery codes are the
only self-service way back in, an org admin clearing a member's credential
(`reset_member_authenticator`) is the only assisted one, and invitations are codes the admin
delivers themselves. `users.email` is a unique key and a label in the audit trail — never a
destination. A `Mailer` trait reappearing here means somebody has reintroduced a dependency
the design removed on purpose.

The console API is read-only over the queue, and a unit test
(`the_queue_is_read_only_over_the_console`) fails if a write ever appears under `/jobs`.
Every job write belongs to `df-mcp`: the agent doing the work is the only party that can
say when it is done, and a "mark complete" button would let a human put something into the
audit trail that they did not observe.

## `web/` — the console UI

Four things hold, and the first explains the other three.

- **It is a single-page app for a security reason, not a performance one.** The session is
  an `HttpOnly`, `__Host-`-prefixed cookie, which browsers refuse to store unless it is
  `Secure`, has `Path=/`, and carries no `Domain` — so it is bound to one origin. A
  SvelteKit *server* rendering these pages would have to hold that credential to fetch on
  the user's behalf: a second process with the keys to every console session, for pages
  behind a login that cannot be cached anyway. `adapter-static` with an `index.html`
  fallback keeps the cookie in the browser and makes CORS a non-question. The same fact is
  why `vite.config.ts` *proxies* `/api`, `/oauth`, and `/.well-known` in development — a
  cross-port `fetch` would not carry the cookie, and no CORS header could rescue it.

- **The server's rules are mirrored, never re-implemented.** A `404` on an org renders as
  "no such organization" and never "you don't have access", because the API answers `404`
  for both cases on purpose. `OrgContext.isAdmin` hides buttons; `OrgCtx` is what refuses
  them. An invitation link opens a *page* that `POST`s — `/invite/{org}` — so a link
  preview following the URL burns nothing.

- **Nothing about the deployment is baked into the bundle.** The MCP endpoint and the
  grantable scopes are read from `/.well-known/oauth-protected-resource` at runtime. A
  hard-coded MCP URL is how a staging build ends up printing a connect command pointing at
  production.

- **Every coding agent gets the same shape.** `src/lib/clients.ts` is one table with one
  entry per client and two forms each (OAuth, access token). A bespoke wizard for one agent
  and a footnote for the rest is the first place constraint 3 would quietly break.

Runes throughout — `$state` / `$derived` / `$props` / `$effect`, no Svelte 4 stores, no
`export let`. Shared state lives in `.svelte.ts` modules (`session.svelte.ts`) or in
context (`org.svelte.ts`); the org is read from the route on every access rather than
copied into state, because a copy and the URL disagree for one frame after a navigation
and that frame is where one org's data renders under another's heading.

Org pages live under `/o/[org]`, not `/[org]`, so no org slug can collide with a page name.
The paths the *server* names — `/login`, `/verify`, `/recover`, `/invite/{org}`,
`/settings/billing` — are fixed by what the server puts in an invitation link and in
`df-billing`'s upgrade prompt, and cannot be renamed here alone.

## `df-server` — assembly, and the two things only it can get wrong

One binary mounts every surface on one port. Nothing here has business logic; what it has is
the decisions no single crate could make.

- **Route collisions are a startup panic, so a test builds the router.** `df-web` and
  `df-mcp` both serve `/.well-known/oauth-protected-resource`, each for a good reason, and
  `Router::merge` panics rather than choosing. `df-server` mounts `df_mcp::mcp_endpoint`
  (the MCP route alone) beside `df-web`'s catalog, and `the_whole_router_assembles` reaches
  that panic before a deployment does.
- **The console SPA is the fallback, but not under `/api`, `/oauth`, `/mcp`, or
  `/.well-known`.** `index.html` answering an unknown path is what makes a hard refresh of a
  deep link work; `index.html` answering `/api/no/such/thing` with `200 text/html` is what
  makes an agent retry forever against a route that will never exist.
- **`/healthz` never touches the database and `/readyz` always does.** They answer different
  questions — "should this process be killed?" and "should traffic come here?" — and wiring
  liveness to the database turns a brief database blip into a simultaneous cold start of
  every replica.
- **`into_make_service_with_connect_info` is load-bearing.** `df_web::state::client_ip`
  reads the peer address out of `ConnectInfo`, and that address keys every per-IP throttle
  and every audit entry. Serve without it and `client_ip` returns `None` for every request,
  silently disabling rate limiting on login and client registration.
- **`Config::from_env` never falls back quietly.** A variable that is *set* but unparseable
  is a startup error naming it, not a default — `DF_ENFORCE_QUOTAS=yes-please` reading as
  "off" is how a billing control gets deployed switched off for a year. `DF_PUBLIC_URL` and
  `DF_ENCRYPTION_KEY` have no defaults at all, because a wrong value for either fails
  silently: bad links in somebody's inbox, or tokens minted for an audience nothing accepts.
- **`DF_CLIENT_IP_HEADER` names the header, and which header is not a matter of taste.**
  Only a header the proxy *overwrites* can be trusted. On Fly.io that is `fly-client-ip`,
  never `x-forwarded-for` — fly-proxy appends, so a caller's own value arrives left-most and
  every throttle keys on something the attacker chose.
- **Graceful shutdown outlives the server.** `axum::serve(...).with_graceful_shutdown(...)`
  returns, and only then does `watcher.shutdown().await` run — see the trap below.

## A trap in tests: the change listener holds a connection

`Watcher::spawn` takes a connection out of the pool for `LISTEN` and **detaches** it, so
dropping the pool does not reclaim it. A `#[sqlx::test]` cannot drop its throwaway database
while a session is still attached, so a test that spawns a watcher and leaves it running
hangs at teardown rather than failing. `Watcher::shutdown()` stops the task and waits for
the connection to go; `Drop` does it best-effort. Anything that needs the connection
released before it continues — graceful shutdown, a test tearing down — calls `shutdown`.

## Migrations

Forward-only, one file per concern, in `crates/df-core/migrations/`. Never edit a migration
that has been applied anywhere; add a new one. `0007_rls.sql` runs last so earlier
migrations' tests are not fighting policies mid-build.

## Authentication

Two layers that must not be conflated:

- **Layer 1 — what a client may do**: OAuth 2.1 (PKCE S256 mandatory, RFC 7591 dynamic
  registration, RFC 8707 resource indicators enforced), plus personal access tokens for
  clients with incomplete OAuth support. Tokens are opaque and stored only as SHA-256
  hashes. A token's org is fixed at issuance and cannot be pivoted.
- **Layer 2 — who the human is**: **passkeys** (WebAuthn) for individuals, enterprise OIDC
  federation for orgs that bind an IdP. No password is ever accepted or stored, and **no
  email is ever sent**.

  A passkey creates the account: `POST /api/auth/signup/start` takes **no body at all**,
  and the address is a profile field set afterwards by someone already holding the key.
  That ordering is what finally closed the account-enumeration oracle — a password leaks
  through "already registered", and TOTP leaked because the secret had to come back in the
  response and so had to be refused for an address that already had one. Nothing is
  submitted before a ceremony, so there is nothing to answer differently about. The one
  place the product says "that address is taken" is `PATCH /api/me`, which needs a session.

  Sign-in is **usernameless**: credentials are discoverable, `allowCredentials` is empty,
  and the browser resolves the account from the key it offers. Verified in a real browser
  with a CDP virtual authenticator — `isResidentCredential: true`, and the login page has
  zero input fields.

Redirect URI matching is exact, with one carve-out: `http://127.0.0.1`, `http://[::1]` and
`http://localhost` ignore the port (RFC 8252 §7.3) and match on everything else. Do not
tighten that without registering the change against a real client first — `localhost` was
once excluded on sound-sounding reasoning, and it silently removed the OAuth path for Claude
Code, which registers `http://localhost:<port>/callback`. `docs/clients/matrix.md` records
what each client actually sends.

**Recovery is a second passkey, and there is no static secret anywhere.** Recovery codes
were dropped rather than carried over: a code that bypasses a phishing-resistant credential
is the weakest link, and keeping one would undo the reason for passkeys. The console pushes
for a second key from the moment there is one (`shouldAddPasskey`), and `passkeys::remove`
refuses to delete the last one — that click looks like tidying up and is a permanent
lockout.

The assisted path is `POST /api/orgs/{org}/members/{user}/reset-passkeys`. **It clears the
keys and issues a claim code in the same operation, and that coupling is the point.** An
account with no passkeys and no outstanding claim is claimable by whoever reaches
registration first — which is exactly the takeover an earlier draft of this endpoint opened,
where a stranger who knew the address could win the race against the member. Assisted
recovery always means the assistant *could* impersonate; the honest mitigations are that it
is auditable, single-use, and expiring. An org's last owner has nobody above them, and the
console says so.

**Two places `df-auth::passkeys` overrides webauthn-rs, both on the challenge and never on
the verification state.** `start_passkey_registration` sets `require_resident_key(false)`,
which would produce credentials that cannot be found without naming the account first;
`start_discoverable_authentication` forces `mediation: conditional`, which is the autofill
flow and shows no prompt. Removing either override silently breaks usernameless sign-in.

**Account resolution is by credential ID, not the user handle.** webauthn-rs offers
`identify_discoverable_authentication`, which reads the handle — the one field an
authenticator may omit, and every software authenticator does. Neither is evidence; the
signature is.

## Metering

The billable unit is the MCP tool call, but the free/billable classification in
`df-billing::classify` is load-bearing: `watch` is a continuous long poll, and billing it
flat would charge an idle agent tens of thousands of calls a month. Record every call
regardless of class so the classification can be repriced without losing history.

Three rules hold, and the first is what makes the other two true:

1. **The meter runs inside the tool's own transaction, before the work.** `Factory::charge`
   is the first thing after `self.tx(...)`. A failed call rolls the meter back with
   everything else, so it is never billed; a successful one has no second transaction to
   retry, so it is never billed twice. `watch` is the one exception — it meters in a short
   transaction of its own, because holding one open across a thirty-second poll would pin
   a connection per idle agent.
2. **A new tool must be classified.** `exhaustive_over` compares the router against the
   price list and `every_tool_has_a_price` fails when they disagree. An unclassified tool
   is treated as free and logged: over-billing a customer for something nobody decided to
   charge for is a worse failure than under-billing ourselves.
3. **Enforcement never blocks a read.** It is behind `DF_ENFORCE_QUOTAS`, off by default,
   and refuses only billable tools on hard-stop plans. An org that runs out mid-task keeps
   full read access to its own queue.

## Style

- Errors are written for an LLM caller that has never read the docs: say what went wrong,
  what the valid options were, and what to call next. `Error::code()` is the stable
  machine-readable branch point; `retriable()` tells an agent whether to back off or
  rethink.
- Comments explain **why**, especially where the obvious implementation is wrong (see
  `Db::begin`, `normalize_remote`, `Watcher::wait`). Do not narrate what the code says.
- No `unwrap()` outside tests. No silent fallbacks on a resolution failure — errors that
  guess are worse than errors that stop.
