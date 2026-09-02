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
  - Role: `dark-factory` (`schema_admin` — owns its own schema only, cannot touch
    `light_factory` or `fly-db`)
  - Attached via `fly mpg attach kyzl60xmdjxopj9g -a dark-factory-mcp -d dark_factory
    -u dark-factory --variable-name DATABASE_URL` — this staged `DATABASE_URL` as an app
    secret.

  **Never** run `fly mpg` commands with a bare cluster-wide scope (e.g. resetting or
  dropping without naming `dark_factory`/`dark-factory` explicitly) — the cluster is
  shared with nels' production database.

- **Secrets staged** (not yet deployed — no image exists yet to receive them):
  `DATABASE_URL`, `DF_ENCRYPTION_KEY`, `DF_SIGNING_KEY` (both generated with
  `openssl rand -base64 32`, per `.env.example`).

## Before the first deploy: pre-provision the `df_app` role

`0007_rls.sql` — the last migration, run by every `df-server` startup —
issues `CREATE ROLE df_app NOLOGIN` and `GRANT df_app TO CURRENT_USER`. Both
are cluster-level operations that need `CREATEROLE`, and the `dark-factory`
role attached above is a `schema_admin`: it owns its own schema and nothing
else, specifically so it *cannot* touch the cluster or other apps' databases.
That means it cannot create `df_app` either, and the very first startup would
fail before binding a port.

This has to be provisioned once, out-of-band, by whoever administers the
`savvagent-pg` cluster (a role with `CREATEROLE`, connected to the
`dark_factory` database):

```sql
CREATE ROLE df_app NOLOGIN;
GRANT df_app TO "dark-factory";
```

`df-server` recognizes a permission-denied failure at this step and fails
startup with this same remediation rather than a bare Postgres error;
migrations are safe to re-run once the grant exists. This is a one-time step
per cluster — a role, once created, is not migration state and is not undone
by anything in this repo.

## What's scaffolded

- `Dockerfile` — multi-stage build of the `df-server` binary, copies
  `crates/df-core/migrations` alongside it for startup migrations.
- `fly.toml` — `dark-factory-mcp` app, region `iad` (co-located with the Postgres
  cluster), `min_machines_running = 1` / `auto_stop_machines = false` (no idle
  scale-to-zero), health check on `GET /readyz`.

## What's assembled (Task 13 in the milestone plan)

`df-server/src/main.rs` now assembles the real server:

1. The Axum router merges df-mcp (`/mcp`, bearer-authenticated) and df-web
   (console API, OAuth AS, `/.well-known/…`), bound to `DF_BIND`. df-mcp's own
   copy of `/.well-known/oauth-protected-resource` is left out of the merge —
   see `df_mcp::mcp_only_router` — since df-web already serves that path and
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

## What a deploy today would and would not be able to do

`/readyz` will pass and the API will work end to end for an MCP client — sign
up via the API, register a client, complete the OAuth code flow, call tools —
but **no browser page exists yet to reach any of it through**. `df-server`
mounts only `/mcp`, the console's JSON API, `/oauth/*`, and the health routes;
nothing serves HTML or static assets, because `web/` (task 11, the SvelteKit
console) is still an empty directory. Concretely:

- `authorize_page`'s redirect for a signed-out visitor goes to
  `/login?next=…`, which 404s.
- Every verification, recovery, and invitation email links to `/verify`,
  `/recover`, or `/invite/{org}`, which all 404.
- There is nothing for a human to click "sign up" on at all.
- Every email — verification, recovery, invitation — is only logged
  (recipient + subject; see `crates/df-web/src/mail.rs`), not delivered, since
  no real mail-provider integration exists yet. A human still cannot get
  through onboarding even by API, because the link they need to click is
  never sent anywhere real. `df-server` requires `DF_ALLOW_LOG_MAILER=1` to
  start at all with this in place, precisely to keep the gap from being
  silent.

None of that blocks getting the server itself live and reachable — which is
what makes it worth doing before task 11 rather than after — but it does mean
a deploy today is only usable by something that speaks the API directly (a
coding agent completing OAuth, or a script hitting the console's REST
endpoints), not by a person in a browser, until task 11 lands.

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
