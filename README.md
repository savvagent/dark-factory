# dark-factory

A hosted, multi-tenant **MCP server for coordinating agentic coding work** across
enterprises and teams.

Server-only: no TUI, no PTY, no local bridge binary, no plugin. A team member adds one
HTTPS endpoint to their coding agent, signs in through the browser once, and from inside
any registered git repository gets tools to add, claim, and complete jobs; to see what
every other agent on the team is working on and where; and to keep GitHub Issues / JIRA in
sync with that work.

```bash
claude mcp add --transport http factory https://mcp.<domain>/mcp
```

That is the entire client-side install.

## Design principles

**It is about coordinating work, anchored on repositories.** A repo is a first-class,
org-owned entity, not a config string. Jobs belong to repos, agents announce which repo and
branch they are in, and the primitives that stop two agents colliding are repo-scoped.

**It is a substrate, not a workflow.** dark-factory deliberately does less than its
ancestor. It provides coordination primitives and ships no opinion about how work should be
specified, planned, reviewed, or measured. Customers encode their own methodology in their
own skills, commands, plugins, and subagents. When a capability could live either in the
server or in a customer's skill calling the server, it belongs in the skill.

**It is coding-agent agnostic.** Claude Code, Copilot CLI, Cursor, Codex, and anything else
that speaks MCP are all first-class. No dependence on any client's hook, plugin, or skill
system; free-form `agentType`; and a personal-access-token path for clients whose OAuth
support is incomplete.

Full design: [`docs/specs/2026-09-01-dark-factory-design.md`](docs/specs/2026-09-01-dark-factory-design.md).
Build order: [`docs/plans/2026-09-01-milestone-1.md`](docs/plans/2026-09-01-milestone-1.md).

## Status

Milestone 1, tasks 2–12 of 13 complete. `df-server` binds a port and serves every surface
on it, and two real coding agents have coordinated on one queue through it — see
[`docs/clients/matrix.md`](docs/clients/matrix.md). What remains of Milestone 1 is the
first live deploy (task 13) and CI (task 1). Milestone 2 (GitHub App + JIRA two-way sync,
plus the console UI for it) is complete —
[`docs/plans/2026-09-03-df-trackers.md`](docs/plans/2026-09-03-df-trackers.md).

| Crate | State |
|---|---|
| `df-core` | ✅ orgs, repos, jobs, leases, messages, change-watch |
| `df-auth` | ✅ OAuth 2.1 AS, TOTP + recovery, PATs |
| `df-mcp` | ✅ Streamable HTTP MCP, 27 tools, resource-server middleware |
| `df-billing` | ✅ usage metering, free/billable split, tier buckets |
| `df-trackers` | ✅ GitHub App + JIRA two-way sync (milestone 2) |
| `df-web` | ✅ console API, session cookies, the AS's browser endpoints, tracker console |
| `df-server` | ✅ config, startup migrations, router assembly, health, deploy |
| `web/` | ✅ SvelteKit 2 / Svelte 5 console |

## Tenant isolation

The product's central claim is that one org cannot see or touch another's data. It rests on
two independent guards, and both are tested:

1. **API shape.** Tenant data is reachable only through `Tx`, which cannot be constructed
   without an `OrgId`, and every statement carries `org_id = $1`.
2. **Row-level security.** Every tenant transaction opens with `SET LOCAL ROLE df_app` and
   `SET LOCAL app.org_id`. A query that forgets its predicate returns nothing rather than
   leaking.

The `SET LOCAL ROLE` is the non-obvious half and the reason guard 2 works at all: Postgres
exempts **superusers and table owners** from their own RLS policies, and the connecting user
is frequently both. `tests/isolation.rs` proves each guard separately — the two
`rls_scopes_*` tests issue deliberately unscoped SQL inside a pinned transaction and fail if
RLS is not in effect. Verified by removing `SET LOCAL ROLE` and confirming exactly those two
tests go red while the other eight stay green.

## Local development

```bash
podman compose up -d                  # Postgres 16 on host port 15433
cp .env.example .env                  # DATABASE_URL for sqlx
cargo test                            # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

Integration tests are `#[sqlx::test]`: each gets a fresh throwaway database with migrations
applied. Port 15433 is deliberately non-standard so it cannot clash with a system Postgres
or with dark-agent's container on 15432.

```bash
cargo test -p df-core --test isolation   # tenant isolation only
cargo test -p df-core --test queue       # queue behaviour only
```

The console has its own gate, which is the same two checks in the other language:

```bash
cd web
npm install
npm run check     # svelte-check, strict
npm run lint      # prettier --check
npm run build     # static bundle into web/build
```

`npm run dev` proxies `/api`, `/oauth`, and `/.well-known` to `DF_API_ORIGIN` (default
`http://127.0.0.1:8080`) so every request stays on one origin — the session cookie carries
the `__Host-` prefix and cannot cross ports. See [`web/README.md`](web/README.md).

### Running the server

```bash
cargo run -p df-server
```

It reads `.env`, applies migrations, and serves everything on one port:

| Path | Surface |
|---|---|
| `/healthz` | Liveness. Never touches the database. |
| `/readyz` | Readiness. Probes the database; `503` when it cannot. |
| `/api/…` | Console REST API (`/api/openapi.json` describes it). |
| `/oauth/…`, `/.well-known/…` | Authorization server and discovery. |
| `/mcp` | The MCP endpoint. Bearer tokens only. |
| everything else | The console SPA, with an `index.html` fallback. |

`DF_PUBLIC_URL` and `DF_ENCRYPTION_KEY` are required and have no defaults, because a wrong
value for either fails silently rather than loudly — see the comments in `.env.example`.
Run `npm run build` in `web/` first, or every console page answers `404` while the API
works perfectly.

## Deployment

```bash
podman build -t dark-factory .
```

One image: the console bundle is built by a `node` stage, the binary by a `rust` stage, and
both land in a `debian-slim` runtime that runs as a non-root user. No database is needed to
build it — every statement in `df-core` is a runtime `sqlx::query` rather than a `query!`
macro, so there is no compile-time schema check and no `.sqlx` offline data to keep current.

On Fly.io, [`fly.toml`](fly.toml) carries the non-secret configuration and the health check.
The rest are secrets:

```bash
fly secrets set \
  DATABASE_URL="postgres://…" \
  DF_ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  DF_PUBLIC_URL="https://factory.example.com"
fly deploy
```

Migrations run at startup under a Postgres advisory lock, so several machines booting
together is safe: the losers wait rather than racing through the same DDL.

Two settings are deployment-specific and easy to get subtly wrong:

- **`DF_CLIENT_IP_HEADER`** decides what every per-IP throttle and audit entry is keyed on.
  Leave it unset with no proxy in front. Behind Fly's proxy it must be `fly-client-ip` and
  **not** `x-forwarded-for`: fly-proxy *appends* to `X-Forwarded-For`, so a caller sending
  its own value arrives left-most, and a rate limiter keyed on that is worse than none
  because it looks like it is working. `fly.toml` sets it.
- **`DF_PUBLIC_URL`** is the audience every token is bound to and the origin every issued
  link points at. It is not derived from the `Host` header on purpose — that header is
  attacker-controlled, and an audience derived from one is not an audience check.

## Repository layout

| Path | What it is |
|---|---|
| `crates/df-core` | Domain + all SQL. Every tenant operation takes an `OrgId`. |
| `crates/df-core/migrations` | Forward-only schema, one file per concern. |
| `crates/df-auth` | OAuth 2.1 AS, TOTP, enterprise OIDC, personal access tokens. |
| `crates/df-mcp` | MCP server and tool surface. |
| `crates/df-billing` | Usage metering and tier enforcement. |
| `crates/df-trackers` | GitHub App + JIRA sync. |
| `crates/df-web` | Console REST API. |
| `crates/df-server` | The binary: config, migrations, router assembly, health. |
| `Dockerfile`, `fly.toml` | The image and its Fly.io deployment. |
| `web/` | SvelteKit 2 + Svelte 5 console. |

## Metering

The billable unit is the MCP tool call, but **not every call is billable**. `watch` is a
30-second long poll every connected agent calls continuously; billing it flat would charge an
idle agent ~86,000 calls a month for doing nothing. Tools are classified as free (reads and
polls) or billable (work); both are recorded so the classification can be repriced later
without losing history. The rule customers are told: **you pay for work performed, not for
looking.**

## Relationship to dark-agent

`dark-agent` is the single-organization ancestor — a TUI hosting a `claude`
PTY plus a queue server authenticated by AWS SigV4 with an IAM-ARN allowlist. dark-factory
takes the server's ideas (job lifecycle, atomic claim, dependency graph, `LISTEN`/`NOTIFY`
watch, message channel, repo registry), re-tenants them, and drops the rest. It is not a fork
and shares no code.
