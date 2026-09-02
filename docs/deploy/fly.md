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

## What's still required before the first real deploy (Task 13 in the milestone plan)

`df-server/src/main.rs` is currently a stub. Before `fly deploy` can succeed:

1. Assemble the real Axum router (df-auth + df-mcp + df-billing + df-trackers + df-web)
   bound to `DF_BIND`.
2. Add `/healthz` (liveness) and `/readyz` (readiness — checks DB connectivity) endpoints.
3. Run pending migrations at startup under a Postgres advisory lock (so multiple
   machines starting concurrently don't race migrations).
4. Structured logging via `tracing-subscriber` (already a dependency).
5. Confirm `DF_PUBLIC_URL` / `DF_RESOURCE_URI` in `fly.toml` once the final hostname
   (Fly default `https://dark-factory-mcp.fly.dev` or a custom domain) is decided.

Once those land, first deploy is:

```bash
fly deploy -a dark-factory-mcp
```

which will build the Dockerfile, run migrations on startup, and pass the `/readyz`
check before routing traffic to the machine.
