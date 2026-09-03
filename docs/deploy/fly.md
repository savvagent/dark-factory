# Hosting: Fly.io

dark-factory runs as a single persistent machine on Fly.io — no scale-to-zero, because
`Watcher::spawn` (df-core) holds a detached `LISTEN` connection that a request-scoped
serverless model would fight. See `CLAUDE.md` for why the process must stay warm.

## Org and infra (provisioned)

- **Org**: `savvagent`.
- **App**: `dark-factory-mcp` (created; `dark-factory` itself is already taken globally
  on Fly.io by an unrelated app, hence the `-mcp` suffix).
- **Database**: the shared `savvagent-pg` managed Postgres cluster
  (`kyzl60xmdjxopj9g`, region `iad`) also serves `light-factory` and `nels-api`. Isolation
  follows the cluster's existing per-app pattern — each app gets its own database and
  role, never a shared one:
  - Database: `dark_factory`
  - Role: `dark-factory` (a member of `schema_admin`). It owns the `dark_factory`
    schema — but **not** only that: see "The app's credential can reach
    `light_factory`" below before treating this as isolation.
  - Attached via `fly mpg attach kyzl60xmdjxopj9g -a dark-factory-mcp -d dark_factory
    -u dark-factory --variable-name DATABASE_URL` — this staged `DATABASE_URL` as an app
    secret.

  **Never** run `fly mpg` commands with a bare cluster-wide scope (e.g. resetting or
  dropping without naming `dark_factory`/`dark-factory` explicitly) — the cluster is
  shared with nels' production database.

- **Secrets staged** (not yet deployed — no image exists yet to receive them):
  `DATABASE_URL`, `DF_ENCRYPTION_KEY`, `DF_SIGNING_KEY` (both generated with
  `openssl rand -base64 32`, per `.env.example`).

## Tenant isolation on managed Postgres — and what the app role can actually reach

Two facts about `savvagent-pg` decide how isolation works here. Both were verified
against the live cluster rather than reasoned about, because the first one was
wrong in an earlier draft of this document.

### `df_app` cannot exist on this cluster, and does not need to

`0007_rls.sql` issues `CREATE ROLE df_app NOLOGIN` and `GRANT df_app TO
CURRENT_USER`. Both are cluster-level operations needing `CREATEROLE`. On this
cluster the only role with it is `postgres`, and `fly mpg connect -u postgres`
answers `cluster … or user postgres not found` — flyctl does not issue those
credentials. `CREATE ROLE df_app NOLOGIN` as `dark-factory` fails with
*permission denied*, and `fly mpg users create` rejects the name outright
(`user_name must contain only lowercase letters, numbers, and dashes`).

That does not weaken isolation, because `df_app` was never the only guard. Every
tenant table is `FORCE ROW LEVEL SECURITY`, which makes the policies apply to the
table's **owner** as well — and connecting as `dark-factory` lands in
`schema_admin`, which owns the tables but is neither a superuser nor `BYPASSRLS`
(`rolsuper=f`, `rolbypassrls=f`). Verified directly against `dark_factory` inside
a rolled-back transaction: an unpinned `SELECT` over a policied table returned
**0 rows**, and the same query pinned to one org returned that org's row only.

So `Db::begin` issues `SET LOCAL ROLE df_app` **only when the role can actually be
assumed**, and `Db::verify_tenant_isolation` re-derives the whole question from the
catalog at startup. `df-server` refuses to bind a port unless one of two things
holds:

- the tenant role was assumed (local development, `#[sqlx::test]` — where the
  connecting role *is* a superuser and dropping out of it is the only thing that
  makes policies bite), or
- the connecting role is neither a superuser nor `BYPASSRLS`, and every tenant
  table is `FORCE`d.

A healthy boot logs which one it is running under:

```
INFO df_server: tenant isolation enforced as role "dark-factory"
                (connecting role, not exempt from RLS); 14 tenant tables, 14 forced
```

The combination those two hide between them — no `df_app` *and* an exempt
connecting role — is a startup error naming the remediation. Nothing about that
check is optional or best-effort: a deployment that cannot prove isolation does
not serve.

> An earlier version of this section said df-server "recognizes a permission-denied
> failure at this step and fails startup with this same remediation". It did not —
> `main.rs` wrapped the failure in a bare `.context("migrations failed")`, and the
> migration aborted before any RLS was applied at all.

### The app's credential can reach `light_factory`

`docs` previously claimed the `dark-factory` role "owns its own schema only,
cannot touch `light_factory` or `fly-db`". **That is not true**, and it matters
because `nels-api` runs on `light_factory`:

```
$ fly mpg connect kyzl60xmdjxopj9g -d light_factory -u dark-factory
 current_database | current_user | session_user
------------------+--------------+--------------
 light_factory    | schema_admin | dark-factory
```

`pg_database.datacl` grants `CONNECT` on all three databases to `schema_admin`,
`writer` and `reader`, and `schema_admin` is a member of `pg_read_all_data` and
`pg_write_all_data` — which are **cluster-wide** attributes, not per-database
ones. So the `DATABASE_URL` staged for this app can read and write nels'
production database.

Nothing in this repository caused that and nothing here should fix it: revoking
`CONNECT` from `schema_admin` or `writer` would alter roles another production app
depends on. It is recorded here because it is the real blast radius of leaking
this app's `DATABASE_URL`, and because the same reasoning rules out
`fly mpg users create -r writer` as a way to get a "least-privilege" app role —
on this cluster a `writer` is not least-privilege in the way the name suggests.

Whoever administers `savvagent-pg` should decide whether per-database isolation is
wanted; until then, treat `DATABASE_URL` as a credential to nels' database too.

## What's scaffolded

- `Dockerfile` — a console stage that builds `web/` into `/srv/console`, a Rust
  stage that builds `df-server`, and a slim non-root runtime holding both. The
  migrations are not copied in: `Db::migrate` uses the `sqlx::migrate!` macro,
  which embeds them in the binary at compile time.
- `fly.toml` — `dark-factory-mcp` app, region `iad` (co-located with the Postgres
  cluster), `min_machines_running = 1` with `auto_stop_machines = "suspend"` so a
  `watch` long poll never pays a cold start, health check on `GET /readyz`.

## What's assembled (Task 13 in the milestone plan)

`df-server/src/main.rs` now assembles the real server:

1. The Axum router merges df-mcp (`/mcp`, bearer-authenticated) and df-web
   (console API, OAuth AS, `/.well-known/…`), bound to `DF_BIND`. df-mcp's own
   copy of `/.well-known/oauth-protected-resource` is left out of the merge —
   see `df_mcp::mcp_endpoint` — since df-web already serves that path and
   Axum panics on two handlers for one path.
2. `/healthz` (liveness, no DB check) and `/readyz` (readiness — `SELECT 1`
   against the pool) are mounted directly in `df-server`.
3. Migrations run at startup via `Db::migrate`, which already takes a Postgres
   advisory lock for the duration (`sqlx::migrate!`'s built-in behavior), so
   several machines starting concurrently on a fresh database wait rather than
   racing through the same DDL.
4. Structured (JSON) logging via `tracing-subscriber`, filtered by `RUST_LOG`.
5. Graceful shutdown on `SIGTERM`/Ctrl+C: stops accepting new connections,
   waits for in-flight requests, then calls `Watcher::shutdown()` to release
   its detached `LISTEN` connection before the process exits.
6. A background sweep loop for the auth tables that would otherwise grow
   without bound (`auth_attempts` is the hot-path one — see
   `df_server::spawn_sweeper`'s doc comment).
7. `Db::verify_tenant_isolation` runs after the migrations and **before the port
   is bound**, so a database that cannot enforce tenant isolation stops the
   process instead of serving. See the section above for the two configurations
   that pass.

## What a deploy can and cannot do

Everything. `/readyz` passes, the API works end to end for an MCP client, and the
console is served — `web/` is built into `/srv/console` by the Dockerfile's console
stage, which `DF_STATIC_DIR` points at.

Onboarding is self-contained: **the product sends no email**. Signing up creates a passkey
in the browser, so an account is created and signed in during a single visit to `/signup`,
with no mail provider in the loop and nothing to configure. Recovery is a second passkey,
or an org admin clearing a member's keys and handing over the one-time code that comes
back. Invitations are codes the admin copies out of the console.

There is consequently no `DF_ALLOW_LOG_MAILER`, no `Mailer`, and no deployment state in
which links go to a log instead of a mailbox.

### `DF_PUBLIC_URL`'s host is now load-bearing in a way it was not before

A passkey is cryptographically bound to the WebAuthn **relying party id**, which
`df-server` derives from `DF_PUBLIC_URL`'s host and asserts at startup (`df_web::relying_party`
refuses to boot on a mismatch rather than failing at somebody's first sign-in).

**Changing that host invalidates every passkey ever registered.** Nothing can soften it;
that is what binding a credential to an origin means. Today the app answers on
`dark-factory-mcp.fly.dev`, so that is the rp_id. Moving the console to a custom domain or
to the Cloudflare Worker hostname in [issue #2](https://github.com/savvagent/dark-factory/issues/2)
means every user re-registers — worth settling *before* anyone but you has an account.

Note also that account creation now requires a browser: there is no scripted signup, so
`docs/clients/matrix.md`'s conformance sequence needs a real browser or a virtual
authenticator for its first step.

Confirming `DF_PUBLIC_URL` / `DF_RESOURCE_URI` in `fly.toml` against the final
hostname (Fly default `https://dark-factory-mcp.fly.dev` or a custom domain) and
running the first live `fly deploy` are still pending a deliberate go — deploying
spends real infrastructure and secrets already staged for this app.

Once that decision is made, first deploy is:

```bash
fly deploy -a dark-factory-mcp
```

which will build the Dockerfile, run migrations on startup, and pass the `/readyz`
check before routing traffic to the machine.
