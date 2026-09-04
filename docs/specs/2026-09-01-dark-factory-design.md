# dark-factory — Design

Status: accepted (2026-09-01)

## Summary

**dark-factory** is a hosted, multi-tenant **MCP server for coordinating agentic coding
work** across enterprises and teams. It is *server-only*: no TUI, no PTY, no local bridge
binary, no plugin to install.

A team member adds one HTTPS endpoint to their coding agent, signs in through the browser
once, and from inside any registered git repository gets tools to add, claim, update, and
complete jobs; to see what every other agent on the team is working on and where; and to
keep GitHub Issues / JIRA in sync with that work.

```
claude mcp add --transport http factory https://mcp.<domain>/mcp
```

That is the entire client-side install.

### Three constraints that shape everything below

**1. It is about coordinating work — and coordination is anchored on repositories.**
dark-factory must know the repos a team works in. A repo is a first-class, org-owned
entity, not a config string: jobs belong to repos, agents announce which repo and branch
they are working in, tracker bindings hang off repos, and the primitives that stop two
agents colliding are repo-scoped. An agent that can't resolve its working directory to a
registered repo gets a clear error telling it how to register one, not a silent default.

**2. dark-factory is a substrate, not a workflow.** It deliberately does *less* than
dark-agent. It provides coordination primitives — a tenant-isolated queue with
dependencies, atomic claiming, repo leases, change notification, and a message channel —
and stops there. It ships no opinion about how work should be specified, planned,
reviewed, or measured. Customers encode their own methodology in their own skills, slash
commands, plugins, and subagents layered on top of these tools. When a capability could
live either in the server or in a customer's skill calling the server, it belongs in the
skill. This is the deciding principle for scope disputes.

**3. It is coding-agent agnostic.** Claude Code, GitHub Copilot CLI, Cursor, Codex, and
anything else that speaks MCP are all first-class. Nothing in the protocol surface, auth,
or data model may assume a particular client: standard Streamable HTTP MCP only, no
dependence on any client's hook/plugin/skill system, no client-specific tool annotations,
free-form `agent_type` on jobs, and a token path for clients whose OAuth support is
incomplete (see *Client compatibility*).

## Relationship to dark-agent

`dark-agent` is the single-organization ancestor: a ratatui TUI
hosting a `claude` PTY, plus a queue server (`manager-mcp`) authenticated by AWS SigV4
with an IAM-ARN allowlist, serving one shared queue for one team inside one AWS account.

dark-factory takes the **server's** ideas, re-tenants them, and trims them. It is not a
fork of the TUI and shares no code with it.

| Taken from `manager-mcp` | Dropped |
|---|---|
| Job model + lifecycle (`pending → in-progress → completed/failed`, with an optional `active` refinement of `in-progress`) | The TUI, the PTY host, and every hook |
| Atomic multi-job `claim` under one transaction | Session rotation / auto-compact controllers |
| Dependency graph (`ready` / `blocked` / `set_dependencies`) | The activity channel + OTLP session tracking |
| `LISTEN`/`NOTIFY` + long-poll `watch` change notification | SigV4 `GetCallerIdentity` auth + ARN allowlist |
| Shared agent message channel | `manager-proxy`, the local signing bridge |
| The repo registry — **promoted to a first-class entity** | The `record_metrics` / `metrics` success-metrics framework |
| | The fs-ci repo-root startup guard and all fs-ci coupling |

The success-metrics framework is dropped on purpose: it encodes one team's definition of
done. Its replacement is a generic `metadata` JSONB field on jobs, which a customer's own
skill can use to record whatever it wants to measure.

## Goals

1. **Zero-install onboarding.** One URL, browser sign-in, no local credentials, no AWS
   account, no binary to install — on any MCP-speaking agent.
2. **Hard tenant isolation.** No query can cross an org boundary. Enforced structurally,
   not by convention.
3. **Repo-anchored coordination.** At any moment a team can answer: which repos do we
   work in, what work is queued per repo, who is in each repo right now, and what is
   blocked.
4. **Usage-metered revenue** with predictable tier buckets.
5. **Trackers are first-class**, not a string field: a job and its GitHub issue or JIRA
   ticket stay in sync in both directions.
6. **Agent-first ergonomics.** Tool descriptions, errors, and defaults are written for an
   LLM caller that has never read the docs.

## Non-goals (this phase)

- Any client-side software. No TUI, no plugin, no proxy.
- Hosting or executing coding agents. Customers run their own sessions; we coordinate them.
- Shipping an opinionated workflow, methodology, or skill library.
- SAML (enterprise SSO is OIDC-only in v1) and SCIM directory sync.
- Self-hosted / on-prem distribution.

## Architecture

```
                   ┌────────────────────────────────────────────┐
  coding agent ───▶│  df-mcp     Streamable HTTP MCP (/mcp)      │
  (any client)     │             OAuth resource server           │
                   ├────────────────────────────────────────────┤
  browser ────────▶│  df-web     console API (/api) + OAuth AS   │
                   ├────────────────────────────────────────────┤
                   │  df-auth    OAuth 2.1 AS · passkeys · OIDC  │
                   │  df-billing metering · tiers · Stripe       │
                   │  df-trackers GitHub App · JIRA 3LO · hooks  │
                   ├────────────────────────────────────────────┤
                   │  df-core    domain + Postgres (org-scoped)  │
                   └────────────────────────────────────────────┘
                                      │
                              Postgres (Aurora/Neon)
```

One binary (`df-server`) mounts every HTTP surface on one port; the crates are a
compile-time layering discipline, not separate services. Split later if load demands it.

### Crates

| Crate | Responsibility |
|---|---|
| `df-core` | Domain model + all SQL: orgs, teams, repos, jobs, leases, messages. Every public function takes an `OrgId`. No HTTP, no auth. |
| `df-auth` | OAuth 2.1 authorization server, passkey (WebAuthn) registration/authentication, enterprise OIDC federation, personal access tokens, token issuance + introspection. |
| `df-mcp` | `rmcp` Streamable HTTP server, the tool surface, the OAuth resource-server middleware. |
| `df-billing` | Usage event recording, period counters, tier limits, Stripe sync. |
| `df-trackers` | GitHub App + JIRA OAuth clients, webhook ingest, two-way sync engine. |
| `df-web` | Console REST API, session cookies, the AS's HTML endpoints (login, consent). |
| `df-server` | Binary: config, migrations, router assembly, graceful shutdown. |

`web/` is the console: **SvelteKit 2 + Svelte 5 (runes) + Tailwind v4**, talking only to
`df-web`'s API. Runes (`$state` / `$derived` / `$props` / `$effect`) throughout — no Svelte 4
stores or `export let`. TypeScript, strict.

## Tenancy model

```
users ──┬── org_members ──── orgs ──┬── teams ── team_members
        │      (role)          │    │
        ├── totp_credentials   │    ├── repos ──┬── jobs ── job_dependencies
        └── access_tokens      │    │           ├── repo_leases
                               │    │           └── tracker_bindings
                               │    ├── messages
                               │    ├── usage_events    (metering)
                               │    ├── subscriptions   (billing)
                               │    ├── tracker_connections
                               │    └── idp_connections (enterprise SSO)
```

- **`users`** is a global identity keyed by verified email. One human, one row, however
  many orgs they belong to.
- **`orgs`** is *the* tenant boundary and the billing entity. An "enterprise" and a "team"
  are the same row with different plans; enterprises additionally have `idp_connections`
  and claimed email domains.
- **`teams`** subdivide an org. Repos and jobs carry a nullable `team_id`; when set, only
  that team's members and org admins see them. When null they are org-wide.
- **Roles** on `org_members`: `owner` (billing, delete org), `admin` (members,
  connections, teams, repos), `member` (use the queue).

### Isolation is structural

`org_id` is `NOT NULL` on every tenant table and participates in every unique key that a
user-facing identifier appears in. Two independent guards:

1. **`df-core` API shape.** Every query function's first parameter is `OrgId`, and every
   statement includes `org_id = $1`. There is no function that can read a job without
   naming an org. A test asserts every tenant-table query text contains `org_id`.
2. **Postgres row-level security.** Each request opens its transaction with
   `SET LOCAL app.org_id = <uuid>`; RLS policies on every tenant table restrict rows to
   that setting. If guard 1 is bypassed by a future query, the database still refuses.

Job ids are per-org and human-readable (`job-42`), drawn from a per-org counter
(`orgs.next_job_seq`, bumped under the row lock inside the insert transaction) rather than
a global sequence — so two orgs both have a `job-1` and neither can enumerate the other's.

## Repos — the coordination anchor

A repo row is what makes an agent's `cwd` meaningful to the server.

| Column | Meaning |
|---|---|
| `slug` | Short handle agents use (`contract-explorer`). Unique per org. |
| `remote_urls` | Every git remote form that identifies this repo, **normalized** (scheme, credentials, trailing `.git`, and SSH-vs-HTTPS differences stripped) so `git@github.com:acme/api.git` and `https://github.com/acme/api` resolve to one row. |
| `provider` | `github` \| `gitlab` \| `bitbucket` \| `other`. |
| `default_branch` | For lease and PR conventions. |
| `team_id` | Optional owning team. |
| `default_agent_type` | Free-form hint (`claude-code`, `copilot-cli`, …) — never enforced. |
| `tracker_binding` | JIRA project keys and/or `owner/repo` for GitHub Issues. |
| `active` | Soft-disable without losing job history. |

**Resolution.** Tools accept an optional `repo` (slug) *or* `remote` (raw git remote URL).
The agent passes whatever it has — `git remote get-url origin` is the expected source —
and the server normalizes and resolves it. Resolution order: explicit slug → normalized
remote match → tracker-binding match from the ticket ref → org default. An unresolvable
repo is an error naming the registered slugs and pointing at `register_repo`; it never
silently falls back to a different repo's queue.

**Repo leases** are the primitive that stops two agents colliding. An agent takes a lease
on `(repo, branch)` for a bounded TTL, renews it while working, and releases it on
completion. Leases are advisory and time-bounded — a crashed agent's lease expires rather
than deadlocking the repo — and `list_leases` answers "who is in this repo right now".
The server never enforces a lease against a git operation it cannot see; it makes
collisions *visible and avoidable*, which is what coordination means here.

## Authentication

Two layers that must not be conflated.

### Layer 1 — MCP client authorization (OAuth 2.1)

dark-factory is both the **Authorization Server** and the **Resource Server** for v1,
implementing the MCP authorization spec:

- `GET /.well-known/oauth-protected-resource` (RFC 9728) — advertises the AS. `df-mcp`
  returns `401` with `WWW-Authenticate: Bearer resource_metadata="…"` so an
  unauthenticated client can discover where to authenticate.
- `GET /.well-known/oauth-authorization-server` (RFC 8414) — AS metadata.
- `POST /oauth/register` (RFC 7591) — dynamic client registration, so an agent can
  register itself without an admin creating a client by hand.
- `GET /oauth/authorize` → human login (below) → consent → code.
- `POST /oauth/token` — `authorization_code` + `refresh_token` grants. **PKCE S256
  required**; no implicit grant, no password grant.
- **Resource indicators (RFC 8707)** are required and enforced: a token is minted for a
  named `resource`, and `df-mcp` rejects any token whose audience is not its own canonical
  URI. This is the confused-deputy defense.

Access tokens are opaque random strings stored only as SHA-256 hashes, 1-hour lifetime,
with rotating refresh tokens. A token carries `(user_id, org_id, scopes)`; the org is
fixed at issuance, so a stolen token cannot be pivoted to another org the user belongs to.

Scopes: `jobs:read`, `jobs:write`, `repos:read`, `repos:write`, `messages`, `trackers`,
`org:admin`.

### Layer 2 — proving who the human is

This is what happens *inside* `/oauth/authorize`, and it is where the two customer
segments diverge.

**Individual team members — passwordless passkeys.** A passkey (WebAuthn) both creates the
account and authenticates it: signup issues no session and takes no request body at all,
and the address is a profile field set afterwards by someone already holding the key. Login
is usernameless — credentials are discoverable, `allowCredentials` is empty, and the
browser resolves the account from the key it offers. No password, TOTP secret, or other
static credential is ever set, stored, or reset.

Details that matter:
- The server stores only public keys and signature counters (`passkeys`); losing the whole
  table lets an attacker sign in as nobody, unlike a shared secret at rest.
- A passkey signs over the origin it was registered to (`DF_PUBLIC_URL`), which is what
  makes it phishing-resistant — nobody can be talked into producing a signature their
  authenticator will only make for the real origin.
- Rate limited per account and per IP, with exponential lockout.
- **Recovery** is a second passkey: the console pushes for one from the moment there is
  one, and removing a credential is refused when it is the last one. There is no recovery
  code and no emailed link — dark-factory sends no mail at all. An org admin clearing a
  member's credential (`reset_member_passkeys`) is the only assisted path, and it
  issues a claim code in the same operation it clears the keys, so the account is never
  left both keyless and unclaimed.
- Nothing is submitted before a ceremony, so there is nothing to answer differently about —
  the enumeration oracle a password or a returned TOTP secret used to leak through is
  closed by construction.

**Enterprises — OIDC federation.** An org admin binds an IdP (Okta, Entra ID, Google
Workspace, any OIDC provider) with issuer, client id, and secret, then claims email
domains and proves control via a DNS TXT record. A login for a claimed domain redirects to
that IdP instead of the passkey ceremony; the returned `sub` is pinned to the user row on
first use. Admins can set `enforce_sso`, disabling individual passkey login for that org's
members.

## Client compatibility

Agent-agnosticism is a compatibility problem, not just a protocol choice. MCP client
support for remote servers is uneven: some clients do Streamable HTTP with full OAuth and
dynamic registration, some do HTTP but only with static headers, some still only do stdio.

Three supported connection paths, in preference order:

1. **Streamable HTTP + OAuth 2.1 + DCR** — the default. Browser sign-in, refresh tokens,
   nothing to copy and paste. Used when the client advertises OAuth support.
2. **Streamable HTTP + personal access token** — for clients with partial or no OAuth. The
   user mints a scoped PAT in the console and the client sends
   `Authorization: Bearer df_pat_…`. PATs carry the same `(user, org, scopes)` claims as
   OAuth tokens, are hashed at rest, have an expiry, and are revocable per-token from the
   console. This path exists so that "which agent are you using?" is never a blocker.
3. **stdio via `npx mcp-remote`** — the community stdio→HTTP shim, documented for clients
   that cannot do remote transports at all. No first-party binary; we document it and
   test against it, we do not ship it.

A conformance matrix in `docs/clients/` records, per client, which path is used, the exact
add-server command, and the last version verified. It is a test artifact: each row is
backed by a manual verification checklist run before release.

## Metering and tiers

Every authenticated tool call writes a `usage_events` row `(org_id, user_id, tool,
billable, ts)` and, when billable, increments an `org_period_usage` counter in the same
transaction as the tool's own work — so a failed call is not billed and a successful one
is never billed twice.

**Not every call is billable.** `watch` is a 30-second long poll every connected agent
calls continuously; billing it flat would charge an idle agent ~86,000 calls a month for
doing nothing and make bills unpredictable. Tools are classified in code:

| Free (reads + polls) | Billable (work) |
|---|---|
| `watch`, `inbox`, `unread_count`, `get_job`, `list_jobs`, `ready`, `blocked`, `stats`, `list_repos`, `list_leases`, `whoami`, `usage` | `add_job`, `update_job`, `delete_job`, `claim_jobs`, `complete_job`, `fail_job`, `repend_job`, `set_dependencies`, `send_message`, `register_repo`, `acquire_lease`, `link_ticket`, `sync_ticket` |

Both kinds are recorded, so the classification can be repriced later without losing
history. The rule stated to customers: **you pay for work performed, not for looking.**

Tiers (monthly billable operations):

| Plan | Included | Overage |
|---|---|---|
| Free | 500 | hard stop |
| Team | 10,000 | metered |
| Business | 100,000 | metered |
| Enterprise | custom | contract |

At 80% of the bucket the server attaches a warning to tool results and emails org admins.
Past the bucket, a Free org's billable tools return a structured MCP error naming the
limit and the upgrade URL; reads keep working, so work already in flight stays readable.

## Tracker integration (two-way)

Per-org `tracker_connections` hold encrypted credentials; per-repo `tracker_bindings` say
which project or issue tracker a given repo's jobs map to.

- **GitHub**: a GitHub App installation. Short-lived installation tokens minted per
  request; no user PAT stored.
- **JIRA**: OAuth 2 3LO with a rotating refresh token, scoped to admin-selected sites.

**Inbound.** Webhooks (`issues`, `issue_comment` from GitHub; Automation webhooks from
JIRA) hit `/webhooks/{provider}`, are signature-verified, and resolve to an org via the
installation/site id. An issue labelled for dark-factory creates or updates a job; closing
the ticket cancels or completes the job.

**Outbound.** Job transitions write back: `claim_jobs` comments that an agent picked the
work up and transitions to In Progress; `complete_job` posts the result summary and
transitions to Done; `fail_job` posts the error and returns the ticket to the backlog.

Sync is idempotent and loop-safe: every write records the resulting remote revision, and
an inbound event carrying a revision we just wrote is dropped rather than re-applied.

## MCP surface

**Repos** — `register_repo`, `list_repos`, `resolve_repo`, `update_repo`.

**Jobs** — `add_job`, `get_job`, `list_jobs`, `update_job`, `delete_job`, `claim_jobs`,
`complete_job`, `fail_job`, `repend_job`, `set_dependencies`, `ready`, `blocked`, `stats`.

**Coordination** — `acquire_lease`, `renew_lease`, `release_lease`, `list_leases`,
`send_message`, `inbox`, `ack_messages`, `unread_count`, `watch`.

**Trackers** — `link_ticket`, `sync_ticket`.

**Org** — `whoami` (identity, org, plan, remaining quota), `usage`.

`watch` is the long poll: it `LISTEN`s on a per-org channel and returns `CHANGED` or
`TIMEOUT`, so agents react to queue changes without polling. Self-authored message
notifications are filtered out of the caller's own wake.

Jobs carry a free-form `metadata` JSONB field. dark-factory never interprets it; it is
where customers' own skills store whatever their methodology needs.

## Web console

SvelteKit 2 / Svelte 5 (runes) / Tailwind v4, talking only to `df-web`:

- **Members**: invite, assign role, remove, force-logout, mint/revoke PATs.
- **Teams**: create, assign members, scope repos and queues.
- **Repos**: register, bind trackers, assign to a team, see live leases.
- **Billing**: current tier, live usage against the bucket, invoice history.
- **Connections**: bind GitHub App / JIRA, bind an OIDC IdP, claim domains.
- **Queue**: read-only view of jobs and the message channel, per repo and per team.

## Deployment

A single container image, Postgres, nothing else stateful. Migrations run at startup under
an advisory lock so concurrent replicas are safe. Secrets (DB URL, encryption key for
secrets at rest, OAuth signing key, Stripe key, GitHub App private key) come from the environment. Target:
Fly.io or ECS behind a TLS terminator; Neon or Aurora for Postgres.

## Testing

- `df-core`: `#[sqlx::test]` integration tests against a real Postgres, one throwaway
  database per test. Every tenant-scoped function gets a **cross-org negative test**
  asserting org B cannot see or mutate org A's row.
- Repo resolution: a table-driven test over remote-URL forms (SSH, HTTPS, with and without
  `.git`, with embedded credentials, host aliases) proving they normalize to one row.
- `df-auth`: passkey ceremony correctness (challenge/response, resident-key overrides),
  PKCE, token audience enforcement, PAT scoping, and the enumeration-resistant signup shape.
- `df-billing`: counter arithmetic, bucket boundaries, the free/billable split.
- `df-trackers`: recorded-fixture tests for webhook signature verification and loop
  suppression.
- `df-mcp`: tool-level tests driving handlers with an injected principal.
- Clients: a per-client manual conformance checklist behind the `docs/clients/` matrix.

## Open risks

1. **OAuth 2.1 + DCR interop across clients** is the highest-uncertainty piece, and it
   varies per agent. Mitigation: milestone 1 proves it against two clients (Claude Code
   and Copilot CLI) before anything else is built, and the PAT path exists as the fallback
   for any client that can't complete the flow.
2. **Tool-call metering vs. customer trust.** The free/billable split fixes the idle-agent
   problem but adds a classification customers must understand. Mitigation: `whoami` and
   `usage` report the remaining bucket; the console shows a live meter.
3. **Repo resolution ambiguity.** Monorepos, forks, and mirrors can present remotes that
   map to several plausible rows. Mitigation: resolution is explicit-first and errors on
   ambiguity rather than guessing.
4. **Lease semantics are advisory.** The server cannot see git operations, so a determined
   agent can ignore a lease. This is documented, not hidden — leases make collisions
   visible; they are not a mutex.
5. **Tracker sync loops.** Mitigated by revision recording, but webhook replay and
   third-party edits during a sync window need soak testing.
6. **Per-org job counters serialize inserts within an org.** Fine at expected volume; move
   to a per-org sequence if a single org's insert rate becomes a bottleneck.
