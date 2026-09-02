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

Milestone 1, tasks 2–11 of 13 complete. Every surface a customer touches exists and is
tested; what is missing is a process — `df-server`'s `main.rs` is still four lines, so
nothing binds a port yet.

| Crate | State |
|---|---|
| `df-core` | ✅ orgs, repos, jobs, leases, messages, change-watch |
| `df-auth` | ✅ OAuth 2.1 AS, TOTP + recovery, PATs |
| `df-mcp` | ✅ Streamable HTTP MCP, 27 tools, resource-server middleware |
| `df-billing` | ✅ usage metering, free/billable split, tier buckets |
| `df-trackers` | ⬜ GitHub App + JIRA two-way sync (milestone 2) |
| `df-web` | ✅ console API, session cookies, the AS's browser endpoints |
| `df-server` | ⬜ config, migrations at startup, router assembly (task 13) |
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
the `__Host-` prefix and cannot cross ports. Until task 13 binds that port there is nothing
behind the proxy. See [`web/README.md`](web/README.md).

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
| `crates/df-server` | The binary: config, migrations, router assembly. |
| `web/` | SvelteKit 2 + Svelte 5 console. |

## Metering

The billable unit is the MCP tool call, but **not every call is billable**. `watch` is a
30-second long poll every connected agent calls continuously; billing it flat would charge an
idle agent ~86,000 calls a month for doing nothing. Tools are classified as free (reads and
polls) or billable (work); both are recorded so the classification can be repriced later
without losing history. The rule customers are told: **you pay for work performed, not for
looking.**

## Relationship to dark-agent

[`dark-agent`](../dark-agent) is the single-organization ancestor — a TUI hosting a `claude`
PTY plus a queue server authenticated by AWS SigV4 with an IAM-ARN allowlist. dark-factory
takes the server's ideas (job lifecycle, atomic claim, dependency graph, `LISTEN`/`NOTIFY`
watch, message channel, repo registry), re-tenants them, and drops the rest. It is not a fork
and shares no code.
