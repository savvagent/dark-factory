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
