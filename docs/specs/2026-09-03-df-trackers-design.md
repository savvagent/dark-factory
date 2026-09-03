# df-trackers design — Milestone 2: GitHub App + JIRA two-way sync

> **Status:** DRAFT — schema foundation implemented; GitHub App, JIRA, webhook ingest,
> outbound sync, and the `link_ticket`/`sync_ticket` MCP tools remain.
> **Implements:** the "Tracker integration (two-way)" and "Trackers — `link_ticket`,
> `sync_ticket`" sections of `docs/specs/2026-09-01-dark-factory-design.md`.
> **Depends on:** Milestone 1 (auth, queue, console skeleton) — merged.

## Premise corrections

- `docs/plans/2026-09-01-milestone-1.md` and `CLAUDE.md` both describe `df-trackers` as a
  one-line `lib.rs` stub. That is still accurate as of this spec: no tracker tables, no
  crypto, no HTTP client code exist yet. This spec starts from zero, not from a partial
  implementation.
- The design doc's crate table describes `df-trackers` as owning "webhook ingest". This
  crate has no HTTP framework dependency (no `axum`) and, per `CLAUDE.md`, "every SQL
  statement lives in `df-core`" and no crate outside `df-web`/`df-mcp`/`df-server` accepts
  inbound HTTP directly. This spec resolves that: the `/webhooks/{provider}` **route**
  lives in `df-web` (the only crate that mounts arbitrary unauthenticated HTTP surfaces
  today, alongside OAuth's HTML endpoints), and it calls into `df-trackers` functions for
  signature verification and event parsing. `df-trackers` owns everything after the HTTP
  layer: verification, parsing, GitHub App / JIRA API clients, and the sync engine that
  reads and writes jobs through `df-core`.

## Scope

**In (this spec, Milestone 2 as a whole):**
- `tracker_connections` (per-org, encrypted credentials) and `tracker_bindings` (per-repo,
  which project/tracker a repo's jobs map to) tables, with the two tenant-isolation guards.
- A GitHub App client: installation token minting, issue/comment API calls.
- A JIRA OAuth 2 (3LO) client: authorization code exchange, refresh-token rotation, issue
  API calls.
- Webhook signature verification (GitHub HMAC-SHA256, JIRA shared-secret) and event
  parsing, called from a new `/webhooks/{provider}` route in `df-web`.
- Inbound sync: a labelled issue creates/updates a job; closing the ticket
  cancels/completes the job.
- Outbound sync: `claim_jobs`/`complete_job`/`fail_job` write back a comment and a state
  transition on the linked ticket.
- Loop-safety: every write records the resulting remote revision; an inbound event
  carrying a revision this server just wrote is dropped.
- Two new MCP tools: `link_ticket`, `sync_ticket` (both `trackers` scope, both billable
  per the existing classification table).
- Console UI for binding a GitHub App installation / JIRA site to an org, and a tracker
  binding per repo (a repo settings addition, not a new page).

**In (this task, Task 1 of the Milestone 2 plan — the only task implemented under this
spec draft right now):**
- The `tracker_connections` / `tracker_bindings` schema, migration, RLS registration,
  cross-org negative tests.
- `df-core::trackers` module: typed rows, CRUD functions taking `OrgId`/`RepoId` through
  `Tx`, exactly like `df-core::repos`.
- A `df-trackers::crypto` module providing the at-rest encryption primitive for
  connection credentials (see §4 — this is a deliberate near-duplicate of
  `df-auth::crypto`, not a shared dependency).
- No GitHub App / JIRA HTTP clients, no webhook route, no MCP tools, and no console UI
  yet — those are later tasks in the Milestone 2 plan (docs/plans/2026-09-03-df-trackers.md).

**Out (all of Milestone 2, all tasks):**
- Any tracker other than GitHub Issues and JIRA (e.g. Linear, Azure DevOps). Not asked
  for; the design doc names only these two.
- A generic "webhook relay" product feature usable for anything other than the two
  named trackers. Substrate-not-workflow: dark-factory ships an opinion about exactly the
  two trackers it names, not a generic webhook receiver customers could build in their
  own skill. A generic inbound-webhook-to-job pipeline for arbitrary providers belongs in
  a customer's own skill, not the server.
- Conflict-resolution UI when a ticket and a job disagree (e.g., a human edits the issue
  title while an agent is mid-task). The design's loop-safety rule (last-writer-wins by
  revision) is the full scope; a merge UI is not asked for and is workflow opinion, not
  substrate.
- Any change to the free/billable classification of *existing* tools.

## Assumptions

- **The webhook route belongs to `df-web`, not a new axum surface in `df-trackers`.**
  Rationale above (Premise corrections). `df-trackers` gains no `axum`/`tower` dependency.
- **Credential encryption is a small duplicated primitive, not a new `df-trackers` →
  `df-auth` dependency.** `df-auth::crypto::{encrypt, decrypt}` (AES-256-GCM, key from
  `DF_ENCRYPTION_KEY`) is auth-domain code by its module's own doc comment ("secret
  encryption" for TOTP-era secrets it still keeps around for passkey-adjacent bookkeeping).
  Reusing it would mean `df-trackers` depends on `df-auth`, which the crate table does not
  list and which would let a tracker-credential bug pull in the entire auth surface's
  transitive dependencies. The primitive itself is ~15 lines of RustCrypto calls with no
  auth-specific logic; duplicating it into `df-trackers::crypto` against the same
  `DF_ENCRYPTION_KEY` env var keeps the crates independent at the cost of one small,
  well-tested duplicate. If a third crate needs it, promoting it to `df-core` at that
  point is the right call — not before there's a real third caller.
- **`tracker_connections` is per-org; `tracker_bindings` is per-repo.** Matches the
  design doc's own wording exactly ("Per-org `tracker_connections` hold encrypted
  credentials; per-repo `tracker_bindings` say which project or issue tracker a given
  repo's jobs map to"). A repo can have zero or one binding per provider; an org can have
  zero or one connection per provider (one GitHub App installation, one JIRA site, for v1
  — multiple installations per org is not asked for and adds a selection UI nobody
  requested).
- **`tracker_bindings.connection_id` is nullable at the schema level but the sync engine
  refuses to activate a binding with no connection.** A repo can declare "this maps to
  JIRA project ACME-123" before an admin has bound JIRA at the org level; the binding
  becomes live only once a connection exists. This mirrors the existing
  `Repo.tracker_binding` free-form column's spirit (present since Milestone 1) without
  removing it — Task 1 does **not** touch the existing `repos.tracker_binding` text
  column; that column stays as the human-readable display hint on the repo row, while the
  new `tracker_bindings` table is the structured, connection-linked source of truth the
  sync engine reads. Reconciling/removing the older free-form column is out of scope for
  this spec (Risks & Open Questions).
- **Provider is an enum shared with `repos.provider`'s spirit but scoped to trackers.**
  `github` | `jira` — not `gitlab`/`bitbucket`/`other`, because only GitHub and JIRA are
  in scope (Scope §Out).
- **No webhook signature secret is stored in `tracker_connections` in plaintext.** GitHub
  App webhook secrets and JIRA Automation shared secrets are encrypted at rest with the
  same primitive as OAuth credentials.

## §1 Schema

New migration `crates/df-core/migrations/0011_trackers.sql`, additive only, no edits to
any applied migration (Non-Negotiable Rule 6 / Load-Bearing Invariant 12).

```sql
CREATE TYPE tracker_provider AS ENUM ('github', 'jira');

CREATE TABLE tracker_connections (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    provider            tracker_provider NOT NULL,
    -- GitHub: the App installation id. JIRA: the cloud site id (from the 3LO
    -- accessible-resources response). Opaque to df-core; df-trackers interprets it.
    external_id         TEXT NOT NULL,
    -- AES-256-GCM ciphertext (nonce || ciphertext || tag, base64), never plaintext.
    encrypted_credentials TEXT NOT NULL,
    -- Encrypted webhook signing secret (GitHub HMAC key / JIRA Automation shared secret).
    encrypted_webhook_secret TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, provider)
);

CREATE TABLE tracker_bindings (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    repo_id             UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    connection_id       UUID REFERENCES tracker_connections(id) ON DELETE SET NULL,
    provider            tracker_provider NOT NULL,
    -- GitHub: "owner/repo". JIRA: project key (e.g. "ACME").
    external_ref        TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, provider)
);

CREATE INDEX tracker_bindings_org_id_idx ON tracker_bindings(org_id);
CREATE INDEX tracker_bindings_connection_id_idx ON tracker_bindings(connection_id);
```

Both tables get `org_id NOT NULL`, are added to the `tenant_tables` array a follow-on
migration extends in `0007_rls.sql`'s pattern (this migration cannot edit `0007_rls.sql`
directly since it already ran; instead `0011_trackers.sql` creates its own
`<table>_tenant_isolation` policies inline, `FORCE ROW LEVEL SECURITY`, matching
`0007_rls.sql`'s shape exactly), and `Db::verify_tenant_isolation`'s discovery-by-naming-
convention picks them up without code changes elsewhere.

## §2 `df-core::trackers`

New module `crates/df-core/src/trackers.rs`, same shape as `repos.rs`:

- `Provider` enum (`Github`, `Jira`) — `sqlx::Type` mapped to `tracker_provider`.
- `TrackerConnection` struct (id, org_id, provider, external_id, encrypted fields as
  opaque `String`s — `df-core` never decrypts; that is `df-trackers`'s job) +
  `FromRow`/`Serialize`/`JsonSchema`.
- `TrackerBinding` struct similarly, plus a `resolve_binding(tx, repo_id, provider)` read
  used later by the sync engine.
- CRUD: `upsert_connection`, `get_connection`, `delete_connection`, `upsert_binding`,
  `get_binding`, `delete_binding` — every function takes `&mut Tx` and an explicit
  `org_id` argument on every statement (Guard 1), consistent with `repos.rs`.
- `df-core::lib.rs` gains `pub mod trackers;`.

## §3 Cross-org negative tests

`crates/df-core/tests/isolation.rs` gains `tracker_connections` and `tracker_bindings` to
its existing per-table cross-org `rls_scopes_*` coverage, following the same pattern as
the `repos` table's existing case: two orgs, a connection/binding created in org A,
unscoped `SELECT`/`UPDATE`/`DELETE` issued with `SET LOCAL app.org_id` pointed at org B
under `SET LOCAL ROLE df_app`, asserting zero rows are visible or mutable.

## §4 `df-trackers::crypto`

`crates/df-trackers/src/crypto.rs` — AES-256-GCM `encrypt`/`decrypt` over
`DF_ENCRYPTION_KEY` (already a required env var per `CLAUDE.md` / `Config::from_env`),
independent implementation from `df-auth::crypto` per the Assumption above. Same
constraints: key never logged, ciphertext format `nonce || ciphertext`, base64-encoded for
storage in the `TEXT` columns above.

## §5 What Task 1 does NOT wire up yet

No `df-trackers` dependency is added to `df-web` or `df-mcp` in this task — `df-core` gains
the schema and typed accessors, and `df-trackers` gains the crypto primitive, but nothing
calls either yet. This is deliberate: Task 1 is reviewable and mergeable in isolation
(schema + tests, no behavior change to any running surface), matching this repo's
failing-test-first, one-task-at-a-time plan discipline. Tasks 2+ (GitHub App client, JIRA
client, webhook route, sync engine, MCP tools, console UI) are recorded in
`docs/plans/2026-09-03-df-trackers.md` and build on this foundation.

## Error Handling & Edge Cases

- A connection upsert with a provider that already has a row for that org replaces it
  (the `UNIQUE (org_id, provider)` constraint backs an `ON CONFLICT DO UPDATE`) — binding
  a new JIRA site replaces the old one rather than erroring, since v1 supports exactly one
  connection per provider per org.
- Deleting a connection sets any dependent bindings' `connection_id` to `NULL` rather than
  cascading their deletion — a repo's declared tracker mapping (`external_ref`) survives
  an admin re-binding the org-level connection; the sync engine (a later task) is what
  decides whether a binding with `connection_id IS NULL` is "configured but inactive".
- Deleting a repo cascades its bindings (`ON DELETE CASCADE` on `repo_id`) — no orphaned
  binding for a repo that no longer exists.

## Risks & Open Questions

- The existing free-form `repos.tracker_binding` text column (Milestone 1) and the new
  structured `tracker_bindings` table now both describe "what tracker a repo maps to".
  Reconciling them (migrating the free-form column's data into the new table, or
  deprecating the column) is deferred to a later Milestone 2 task once the sync engine
  exists to consume the structured table — doing it in Task 1 would be a breaking change
  to a public-ish field with no consumer yet to validate against.
- One-connection-per-provider-per-org is a v1 simplification. If a customer legitimately
  needs two GitHub App installations in one org (e.g. two different GitHub orgs), this
  schema needs revisiting. Not raised in the design doc or the task brief; flagged here
  rather than solved speculatively.
