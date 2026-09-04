# df-trackers design — Milestone 2: GitHub App + JIRA two-way sync

> **Status:** DRAFT — schema foundation, GitHub App/JIRA clients, webhook ingest
> (including the org-resolution design in §5a), and the two-way sync engine (§6) are
> implemented; the `link_ticket`/`sync_ticket` MCP tools and the console UI for
> connections/bindings remain.
> **Implements:** the "Tracker integration (two-way)" and "Trackers — `link_ticket`,
> `sync_ticket`" sections of `docs/specs/2026-09-01-dark-factory-design.md`.
> **Depends on:** Milestone 1 (auth, queue, console skeleton) — merged.

## Goal & Success Criteria

Give Milestone 2 a schema foundation for two-way tracker sync that a later task can build
GitHub App and JIRA clients, a webhook route, and the `link_ticket`/`sync_ticket` MCP
tools on top of, without any of those later tasks needing a second migration for a
missing column or a second crypto implementation. Success for Task 1 specifically:

- `tracker_connections` and `tracker_bindings` exist, are `NOT NULL org_id`, are
  registered under `FORCE ROW LEVEL SECURITY` with `<table>_tenant_isolation` policies,
  and `Db::verify_tenant_isolation` vouches for both at startup with no code change.
- A cross-org negative test exists for each new table and fails before the RLS policy is
  added (proving it isn't a false positive).
- `df-core::trackers` CRUD compiles and is exercised by `#[sqlx::test]` integration tests
  with no mocks, matching `repos.rs`'s existing test shape.
- `Cipher`/`Sealed` live in `df-core::crypto`; `df-auth` no longer defines them; every
  existing `df-auth` test that exercised them (moved, not deleted) still passes from its
  new location.
- `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --all --check` are green.

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
- Promoting `Cipher`/`Sealed` (AES-256-GCM secret sealing) from `df-auth::crypto` to
  `df-core::crypto` — the primitive `tracker_connections.encrypted_credentials` needs —
  and deleting the now-dead copy from `df-auth` (see §4).
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
- **The AES-256-GCM sealing primitive (`Cipher`/`Sealed`) moves from `df-auth::crypto` to
  `df-core::crypto`, rather than being duplicated into `df-trackers`.** Verified against
  the code: `df-auth::crypto::Cipher` and `Sealed` exist today but have **zero production
  callers inside `df-auth`** — they are a leftover from the TOTP era (removed by the
  `passkeys/webauthn` PR) and are exercised only by their own unit tests. `df-auth`
  already depends on `df-core`, so promoting this primitive to `df-core::crypto` (adding
  two generic `Error::Crypto` / `Error::Config` variants there) lets `df-trackers` use it
  directly with no new inter-domain dependency, and lets `df-auth`'s copy be deleted
  rather than kept as dead code. This corrects the original draft's plan to duplicate the
  primitive into `df-trackers::crypto` — a second implementation of key parsing,
  nonce handling, and ciphertext format was the wrong call once a second real caller
  existed. `Cipher`/`Sealed` move; `df-auth::crypto`'s token generation, hashing, and
  prefixes (`generate`, `hash`, `verify`, `prefix::*`) are genuinely auth-domain and stay
  put.
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
  `repos.tracker_binding` column's spirit (present since Milestone 1) without removing
  it — Task 1 does **not** touch the existing `repos.tracker_binding` column. Verified
  against the schema: it is `jsonb NOT NULL DEFAULT '{}'::jsonb`
  (`crates/df-core/migrations/0002_repos.sql`), typed as `serde_json::Value` on `Repo`
  (`crates/df-core/src/repos.rs`) — a free-form JSON blob, not plain text as an earlier
  draft of this spec said. That column stays as the display/hint field on the repo row;
  the new `tracker_bindings` table is the structured, connection-linked source of truth
  the sync engine reads. Reconciling/removing the older JSON column is out of scope for
  this spec (Risks & Open Questions).
- **GitHub App credentials are deployment config, not a per-org secret — `tracker_connections`
  does not store the GitHub App private key or its webhook secret.** Verified against the
  design doc's own config section: "OAuth signing key, Stripe key, GitHub App private key
  (…) come from the environment." One GitHub App is registered once by the dark-factory
  operator; an org's `tracker_connections` row for `github` records only the
  **installation id** it was granted (`external_id`) — the thing that varies per org.
  Minting an installation access token needs the single global private key
  (`DF_GITHUB_APP_PRIVATE_KEY`, a later task's config addition) plus that installation id;
  it never needs a per-org secret. Likewise the GitHub webhook HMAC secret is one value
  set on the App itself (`DF_GITHUB_APP_WEBHOOK_SECRET`), not one per installation. JIRA's
  OAuth 2 3LO flow is the opposite shape: the **refresh token is genuinely per-org**
  (each org authorizes its own JIRA site), so `encrypted_credentials` is where it lives.
  Both `encrypted_credentials` and `encrypted_webhook_secret` are therefore **nullable**
  (§1) rather than `NOT NULL` as an earlier draft had them: `NULL` for GitHub (nothing
  per-org to encrypt beyond the installation id already in `external_id`), populated for
  JIRA. `encrypted_webhook_secret` specifically stays reserved-but-unused for both
  providers in Task 1 — GitHub's is global env config and JIRA Automation's webhook
  authentication shape is a later task's decision once the webhook route is being built;
  the column exists now so the schema does not need a second migration to add it.
- **Provider is an enum shared with `repos.provider`'s spirit but scoped to trackers.**
  `github` | `jira` — not `gitlab`/`bitbucket`/`other`, because only GitHub and JIRA are
  in scope (Scope §Out).
- **If a webhook secret is ever stored in `tracker_connections`, it is never plaintext.**
  For Task 1 this is vacuous — GitHub's webhook HMAC secret lives only in
  `DF_GITHUB_APP_WEBHOOK_SECRET` and is never written to `encrypted_webhook_secret` at
  all (that column stays `NULL` for `github` rows). The column exists for a later task's
  JIRA Automation secret, which — if that design ends up needing one — would go through
  the same `Cipher::seal` primitive as `encrypted_credentials`, never plaintext. This
  bullet is the encryption invariant for the column's future use, not a claim that
  anything writes to it today.

## §1 Schema

New migration `crates/df-core/migrations/0011_trackers.sql`, additive only, no edits to
any applied migration (Non-Negotiable Rule 6 / Load-Bearing Invariant 12).

```sql
CREATE TYPE tracker_provider AS ENUM ('github', 'jira');

CREATE TABLE tracker_connections (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    provider            tracker_provider NOT NULL,
    -- GitHub: the App installation id (the private key that mints tokens from
    -- it is deployment config, DF_GITHUB_APP_PRIVATE_KEY, never stored here).
    -- JIRA: the cloud site id (from the 3LO accessible-resources response).
    -- Opaque to df-core; df-trackers interprets it.
    external_id         TEXT NOT NULL,
    -- AES-256-GCM ciphertext, canonically base64(nonce || ciphertext) — see §4
    -- for the encode/decode contract between this single column and
    -- Cipher::Sealed's two-field shape. Never plaintext.
    -- NULL for GitHub (nothing per-org to encrypt: installation id above is
    -- not a secret). Holds the JIRA OAuth refresh token for `jira`.
    encrypted_credentials TEXT,
    -- Reserved for a per-connection webhook secret. NULL for both providers in
    -- Task 1: GitHub's webhook HMAC secret is a single App-level value
    -- (DF_GITHUB_APP_WEBHOOK_SECRET, deployment config); JIRA's webhook
    -- authentication shape is decided in the task that builds the webhook
    -- route. The column exists now so that decision does not need a migration.
    encrypted_webhook_secret TEXT,
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

## §4 `df-core::crypto` (promoted from `df-auth::crypto`)

`df-auth::crypto::Cipher`/`Sealed` (AES-256-GCM over `DF_ENCRYPTION_KEY`, already a
required env var per `CLAUDE.md` / `Config::from_env`) move verbatim to
`crates/df-core/src/crypto.rs`. `df-core::Error` gains two generic variants,
`Config(String)` (bad/missing key material) and `Crypto(String)` (seal/open failure —
tampered ciphertext or a rotated key), matching the wording `df-auth::crypto`'s tests
already assert. `df-auth::crypto.rs` deletes `Cipher`/`Sealed` and their tests (moved,
not duplicated) and keeps everything genuinely auth-domain: `generate`, `hash`, `verify`,
`prefix::*`. `df-trackers` depends on `df-core` already (see Cargo.toml) and calls
`df_core::crypto::Cipher` directly — no new inter-domain dependency, no duplicate
implementation.

**Canonical storage encoding (resolves a §1/§4 inconsistency from the first draft).**
`Cipher::seal` returns a `Sealed { ciphertext: Vec<u8>, nonce: Vec<u8> }` — two values,
by design, so a caller free to use two database columns can. The `TEXT` columns in §1
are single-column, so `df-core::trackers`'s CRUD functions (not `Cipher` itself) own the
encoding contract: `base64(nonce || ciphertext)` on write, split at the fixed 12-byte
nonce prefix and re-assemble into `Sealed` on read. This encode/decode pair lives as two
small private helpers in `crates/df-core/src/trackers.rs` (`encode_sealed`/
`decode_sealed`), not in `df-core::crypto` itself — `Cipher`/`Sealed` stay storage-agnostic
so a future caller that does have two columns available is not forced through this
concatenation.

## §5a Webhook org resolution (Task 3 premise correction)

Task 3's checklist in the plan says "resolve org via installation id / site id →
`tracker_connections`" as if this were an ordinary tenant-scoped read. It is not, and the
gap is worth naming precisely: `tracker_connections` is registered under `FORCE ROW LEVEL
SECURITY` with an `org_id = current_org()` policy (§1), and every `df-core::trackers`
accessor takes `&mut Tx<'_>` — which cannot be constructed without an `OrgId` already
known. A webhook delivers only a provider-native identifier (GitHub's installation id,
JIRA's site id); the org is exactly the thing being looked up. There is no `OrgId` to pin
a `Tx` to yet, so the normal accessors cannot answer this question at all — an unscoped
query through the RLS-scoped path returns zero rows on any deployment where `app.org_id`
is unset (which is every request before the org is resolved).

This is the same bootstrap problem `CLAUDE.md`'s auth tables solve: "authentication has to
resolve a principal BEFORE an org is known, so pinning them to `current_org()` would make
login impossible" (`0007_rls.sql`'s comment on `access_tokens` et al.). `df_auth::tokens::introspect`
resolves this by never enabling RLS on `access_tokens` at all, and querying `db.pool()`
directly — the table is simply outside the tenant_tables array, so no policy exists to
consult regardless of role or deployment shape.

**Decision: a narrow, secret-free reverse index, not a blanket RLS exemption on
`tracker_connections`.** Excluding all of `tracker_connections` from RLS (mirroring
`access_tokens` exactly) would also expose `encrypted_credentials` — the sealed JIRA
refresh token — to any unscoped query, which is a strictly larger blast radius than the
bootstrap problem requires. Instead, a new migration
(`crates/df-core/migrations/0012_tracker_connection_index.sql`) adds:

```sql
CREATE TABLE tracker_connection_index (
  provider      tracker_provider NOT NULL,
  external_id   text             NOT NULL,
  org_id        uuid             NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  connection_id uuid             NOT NULL REFERENCES tracker_connections (id) ON DELETE CASCADE,
  PRIMARY KEY (provider, external_id)
);
```

No RLS is enabled on this table — deliberately, by the same reasoning as `access_tokens`,
and it is never added to `0007_rls.sql`'s or `0011_trackers.sql`'s `tenant_tables` arrays.
It holds nothing secret: a provider tag, the provider's own (non-secret) installation/site
id, and the two ids needed to say which org and which connection row own it. `df-core`
maintains it transactionally alongside the real row, inside the same `Tx` that writes
`tracker_connections`, so the two can never drift:

- `upsert_connection` additionally upserts the matching `tracker_connection_index` row
  (same transaction — atomic with the connection write).
- `delete_connection` additionally deletes the matching index row.

A new function, deliberately **not** taking a `Tx` (there is no org to pin one to yet):

```rust
/// Resolve which org owns a provider connection, from the provider's own
/// identifier alone. This is the one place a tracker lookup runs before an
/// `OrgId` is known — analogous to `df_auth::tokens::introspect` resolving a
/// principal before a session exists. It reads only `tracker_connection_index`
/// (no RLS, no secret columns) and returns an `OrgId` for the caller to build
/// a normal `Tx` from for every subsequent step. Never add a second unscoped
/// accessor for `tracker_connections` itself — this function is the only
/// place a tracker table is read without an org already pinned.
pub async fn resolve_connection_org(
    db: &Db,
    provider: Provider,
    external_id: &str,
) -> Result<Option<OrgId>>
```

The webhook route in `df-web` calls this once per request to learn the org, then opens a
normal `Tx` for that `OrgId` and uses the existing `Tx`-scoped accessors
(`get_connection`, `resolve_binding`, …) for everything else — the unscoped path is a
one-hop bootstrap, never a substitute for the tenant-isolated one. This keeps guard 1 and
guard 2 both intact for every read of `encrypted_credentials`; the only table ever read
without an org pinned is the index, and it has nothing in it worth stealing.

**Cross-org expectation, tested but not RLS-enforced:** because this table is deliberately
outside RLS (like `access_tokens`), the guarantee that org A's webhook cannot resolve to
org B's connection rests entirely on the `(provider, external_id)` primary key and on
`upsert_connection`/`delete_connection` being the only writers, both of which run inside a
pinned `Tx` and write `tx.org()` — not on a policy. The test for this is a `#[sqlx::test]`
that upserts connections for two different orgs and asserts `resolve_connection_org`
returns each org's own id and never the other's, not a `rls_scopes_*`-style unscoped-SQL
test (there is no policy to probe). This is the same distinction `CLAUDE.md` draws between
guard-1-only tests and true RLS tests — name it in the test's own comment so a future
reader does not mistake the missing RLS test for an oversight.

## §6 Two-way sync engine (Task 4)

This section is the Task 4 design: the concrete rules "an issue labelled for dark-factory
creates or updates a job" and "job transitions write back" resolve to, and the concrete
loop-safety mechanism. Read alongside §5a — Task 4 is the first consumer of
`WebhookEvent` (parsed there) and the first place a live `Tx` exists around a tracker
client call (closing Task 2's deferred JIRA-refresh-token write-back and GitHub
`installation_id` bridging, per the plan's Task 2 "Remaining" note).

**Where this lives.** `crates/df-trackers/src/sync.rs` holds the pure inbound-mapping
logic (`WebhookEvent` → what job operation, if any) and the pure outbound-mapping logic
(a job transition → what comment text and target ticket state) — no SQL, no HTTP, per
`df-trackers`'s existing shape. The `df-web` webhook handler calls the inbound half inside
its existing per-request `Tx` (§5a already opens one after `resolve_connection_org`). The
outbound half is called from `df-mcp`'s `claim_jobs`/`complete_job`/`fail_job` handlers,
but — see "Outbound" below — **after** that `Tx` commits, not inside it: an external HTTP
call has no place holding a Postgres transaction open, and a tracker outage must not be
able to block an agent from claiming or finishing work it already owns in this server's
own queue.

**Trigger label is per-binding config, not a hardcoded string.** `tracker_bindings` gains
`trigger_label TEXT NOT NULL DEFAULT 'dark-factory'` (additive column, sane default —
every binding created before this migration behaves exactly as if it had been set
explicitly). The *mechanism* — "a label on the ticket is what makes the sync engine
notice it" — is the substrate decision this design makes; the specific string is
admin-configured per repo binding, so no org is forced to spell their trigger convention
the same way another org does, and a customer who wants zero label-driven creation can
set it to a value they will never apply. This is the substrate/workflow line drawn
correctly: dark-factory ships the "labels gate inbound job creation" mechanism, not an
opinion about what any specific label should be named.

**Inbound — job creation and update.** On a GitHub `issues` event with `action` in
`{opened, edited, labeled}` (JIRA: any Automation "issue updated"-shaped event), where
`event.issue.labels` contains the resolved binding's `trigger_label`
(case-insensitive exact match):

1. Resolve the repo binding via `df_core::trackers::find_binding_by_external_ref(tx,
   event.provider, &event.binding_external_ref)` (already exists, Task 3's
   loud-on-ambiguity version — this is a `tracker_bindings` lookup by the *webhook's*
   external reference, distinct from `resolve_binding`, which looks up by `repo_id`
   for the outbound direction; both already exist in `df-core::trackers` and neither is
   duplicated by this task). No binding, or a binding with `connection_id IS NULL`
   (declared but not yet activated, per the existing Assumptions section), or a binding
   whose `trigger_label` the event's labels do not contain → the event is acknowledged
   (still `200`, matching the existing "verified but not actionable" shape) and silently
   dropped; this is not an error, since an org may label issues in a repo it has not
   finished configuring, or with a label that means nothing to dark-factory.
2. Compute the job-lookup `ticket_ref`: for JIRA, `event.issue.reference` directly
   (`"PROJ-123"` — already matches the format `add_job`'s doc comment names). **For
   GitHub, `format!("{}#{}", event.binding_external_ref, event.issue.reference)`**
   (`"acme/api#42"`) — `IssueSnapshot::reference` alone is bare `"42"`
   (`payload.issue.number.to_string()` in `parse_github`), which does not match the
   `acme/api#42` convention `add_job`'s own doc comment already documents for
   manually-queued GitHub jobs; constructing the full form here is what makes an issue a
   customer already queued by hand (`add_job` with `ticket_ref: "acme/api#42"`) and the
   same issue arriving over the webhook resolve to the same job instead of silently
   creating a duplicate.
3. **New accessor**, scoped correctly (existing `get_job_by_ticket` takes only
   `ticket_ref`, org-wide, with no `repo_id`/`tracker` filter — safe for its current
   caller but not for this one, since ticket refs are never guaranteed unique across
   repos in one org): `df_core::jobs::get_job_by_ticket_for_repo(tx, repo_id, tracker,
   ticket_ref) -> Result<Option<Job>>`, `WHERE org_id = $1 AND repo_id = $2 AND tracker =
   $3 AND ticket_ref = $4 ORDER BY created_at DESC LIMIT 1` — same "newest wins on a
   duplicate" tolerance the existing function already documents, just properly scoped.
4. **No existing job** → `add_job`-equivalent creation: `title = event.issue.title`,
   `description = event.issue.body`, `ticket_ref` = the value from step 2, `tracker =
   event.provider`, `metadata = {}`. New function `df_core::jobs::create_from_ticket`
   (thin wrapper around the existing insert path `NewJob` already uses — no new SQL
   shape, just a constructor that also sets `tracker`/`remote_revision`, which `add_job`'s
   MCP-facing `NewJob` intentionally leaves unset today).
5. **Existing job, status `Pending` or `InProgress`** → update `title`/`description` from
   the current snapshot. A terminal job (`completed`/`failed`) is left alone — a closed
   ticket re-edited afterward does not resurrect a job the agent already finished;
   scope's "no conflict-resolution UI" applies here too.
6. **GitHub `action == "closed"`, or JIRA event where `event.issue.state`
   case-insensitively matches one of a small fixed closed-vocabulary set** (see below) —
   resolve the existing job (step 3); if found and its status is `Pending` or
   `InProgress` (a job already `Completed`/`Failed` is left alone, same as step 5):
   - GitHub: `event.issue.state_reason == Some("not_planned")` → `fail_job`-equivalent;
     otherwise (`Some("completed")` or `None`) → `complete_job`-equivalent.
   - JIRA: `event.issue.state` case-insensitively matching `{"done", "closed", "resolved"}`
     → `complete_job`-equivalent; matching `{"won't do", "wont do", "cancelled",
     "rejected", "declined"}` → `fail_job`-equivalent. JIRA workflow status names are
     per-project-configurable, so this is a fixed heuristic vocabulary, not an exhaustive
     enum — an unrecognized-but-closed-looking status is scope for a future task, not
     silently guessed at here; **an unrecognized status is treated as "not a close event"
     and step 6 is skipped entirely** (the job is left as-is, having already had steps
     1–5 applied), rather than defaulting to either complete or fail, which would be
     exactly the kind of guess `CLAUDE.md` prohibits.
   - **A resolved job whose current status is `Pending`** (nobody ever claimed it before
     the ticket closed) cannot go through the existing `complete_job`/`fail_job` MCP-tool
     functions — both require `Status::InProgress` (`crates/df-core/src/jobs.rs`'s
     `finalize`), a precondition this design does not relax for those two public
     functions. **New `df-core` function `jobs::close_from_ticket(tx, id, to: Status,
     result: Option<&str>, error: Option<&str>) -> Result<Job>`** allows the transition
     from `Pending` *or* `InProgress` to `Completed`/`Failed`, used only by this sync
     path — the `claim_jobs`/`complete_job`/`fail_job` MCP tools keep their existing
     preconditions unchanged, since "claiming is what makes work yours" (this repo's own
     framing) must still hold for every *agent-driven* transition; a ticket closing out
     from under an unclaimed job is the one case where the ticket itself is authoritative
     over a state the queue had not yet observed any agent activity on.
   - **`IssueSnapshot` gains a new `state_reason: Option<String>` field** (GitHub's
     `issue.state_reason`; unused for JIRA, which uses `state` directly per above) —
     additive to a type introduced in Task 3 that has exactly one caller (`df-web`'s
     webhook route) and zero external consumers, so this is a same-task extension, not a
     breaking change to a shipped interface.
7. Every apply in steps 4–6 also writes `jobs.remote_revision` to the normalized form of
   `event.issue.updated_at` (see below) in the same `Tx` as the job mutation — one write,
   not two. Step 5 alone (an edit with no close) still writes it, so a later close event
   for the same edit is correctly seen as not-newer and would be dropped if redelivered.

**`issue_comment` events are not applied by this task.** They arrive (Task 3 already
parses them) and are acknowledged, but the sync engine's steps above only fire on
`WebhookEventKind::Issue`. Comment-triggered job actions are not asked for by the design
doc's own wording ("An issue labelled for dark-factory creates or updates a job; closing
the ticket cancels or completes the job") and adding one would be speculative scope.

**Loop-safety: `jobs.remote_revision`.** A new nullable `TEXT` column on `jobs`
(migration `crates/df-core/migrations/0013_jobs_remote_revision.sql`, additive — a new
optional field on an already-public type per Non-Negotiable Rule 6, no version bump, no
breaking-change writeup needed; no new RLS policy needed either, since `jobs` is already a
tenant table under an existing policy that governs the whole row, not per-column). Holds
the last remote timestamp this server either *observed* (inbound apply) or *caused*
(outbound write), **normalized to RFC 3339 UTC** so lexical comparison is safe — both
providers' native timestamps parse cleanly with `chrono::DateTime::parse_from_rfc3339`
(GitHub's `issue.updated_at` and JIRA's `fields.updated` are both RFC 3339 already; this
normalization step exists so a comparison never silently trusts an un-verified assumption
about a provider's exact offset/precision formatting). A value that fails to parse is
treated the same as "no revision" below — never propagated as an opaque raw string that
`<=` would compare byte-wise against a normalized one. **`IssueSnapshot` gains
`updated_at: Option<String>`** (raw provider string, parsed at the point of comparison/
storage, not at parse time — keeping `df-trackers::webhook` provider-format-agnostic per
its existing design) for this, populated by both `parse_github` and `parse_jira`.

- **Inbound guard:** before applying steps 4–6, parse both the job's stored
  `remote_revision` (if any) and `event.issue.updated_at` (if any) as RFC 3339. If both
  parse and the incoming value is `<=` the stored one, drop the event — it is our own
  write echoing back, or a stale/out-of-order redelivery. If either is missing or fails to
  parse, the event is still applied and `remote_revision` is left unchanged rather than
  cleared (not overwritten with an unparseable value); an unguarded apply is a bounded,
  self-correcting risk (the next well-formed event still compares correctly), while
  clearing a known-good revision would make every *subsequent* well-formed event look
  newer than it is and reopen the loop permanently. Documented in Risks & Open Questions.
- **Outbound guard:** the follow-up write described under "Outbound" below records the
  resulting `updated_at` into `jobs.remote_revision` in its own short `Tx`, taken
  immediately after the tracker write succeeds. This is what makes the webhook GitHub/JIRA
  delivers moments later (an echo of the write this server just made) compare as `<=` and
  get dropped by the inbound guard above, instead of re-triggering the sync engine's own
  comment-and-transition as if a human had done it.

**Outbound — job transitions write back, best-effort, after commit.** Only jobs with both
`tracker` and `ticket_ref` set attempt a tracker write; a job with neither (the common
case — most jobs have no ticket) is a no-op, checked first, before any tracker client is
constructed. Resolving which connection to use uses the existing
`df_core::trackers::resolve_binding(tx, repo_id, provider)` (already shipped in §2 — no
new accessor needed here, correcting an earlier draft of this section that proposed a
redundant `binding_for_repo`), and a binding with `connection_id IS NULL` makes the
write-back a no-op (declared, not active) rather than an error — a job can exist and be
worked without its ticket ever being reachable, matching the inbound side's same
tolerance.

**Sequencing, and why it changed from an earlier draft of this section:** `claim_jobs`/
`complete_job`/`fail_job` keep their existing shape exactly — `self.tx(...)` → `charge`
→ the `df-core::jobs` status transition → `commit()` — completely unchanged. **After**
that commit succeeds and the MCP response value is in hand, the tool handler performs the
tracker write-back as a *separate* step: resolve the binding and connection by opening a
second, short `Tx` (read-only for this part), release it, make the tracker HTTP call
outside any `Tx`, then open a third short `Tx` solely to write `jobs.remote_revision` (and,
for JIRA, a rotated `encrypted_credentials` — see below) and commit. **A tracker-write
failure is logged at error level and does not fail the tool call** — the job's own
transition in the queue already succeeded and already is the billed action; an agent must
not be blocked from claiming or finishing work it owns because a third-party API is slow
or down. This corrects an earlier draft's design, which held the tracker write inside the
same `Tx` as the status change specifically so a tracker failure would roll back the
billed call — that shape requires holding a Postgres transaction open across an
uncontrolled external HTTP round-trip (a genuine connection-pool-exhaustion risk under
load, the same class of concern `CLAUDE.md` already documents for `Watcher::spawn`'s
long-lived `LISTEN` connection) and conflates two different failure domains: "did the
queue transition happen" (yes, always, if the MCP call returned success) and "did the
ticket also get updated" (best-effort, may lag or fail). The metering charge in
Invariant 11 still holds exactly as designed — it is charged for the queue operation,
which is what actually happened, not for a ticket write-back that is now explicitly
allowed to fail independently.

- `claim_jobs` → for each claimed job with an active binding: post a comment
  (`args.agent.map(|a| format!("Claimed by {a}.")).unwrap_or_else(|| "Claimed.".into())`)
  and, JIRA only, attempt a transition to a status named `"In Progress"`
  (case-insensitive exact match against the transitions the JIRA REST API reports as
  reachable from the issue's current status — `crates/df-trackers/src/jira.rs` gains a
  `list_transitions`/`transition_issue` pair for this, since today's client has neither).
  GitHub has no built-in "in progress" issue state, so GitHub is comment-only here.
  If no matching JIRA transition is reachable, skip the transition and post the comment
  only, logging a warning — a workflow mismatch nobody asked this server to solve is not
  a `claim_jobs` failure.
- `complete_job` → post the result summary as a comment
  (`args.result.unwrap_or("Completed.")`), then: GitHub — close the issue with
  `state_reason: completed`; JIRA — attempt a transition to a status the workflow's
  transition graph reports in the `done` status category (JIRA's transition API exposes
  each candidate transition's target status category; match on category, not name, since
  "Done"-category status names vary more than "In Progress" does in practice) reachable
  from the current status, else comment-only + warning, same tolerance as above.
- `fail_job` → post the error as a comment (`args.error.unwrap_or("Failed.")`), then:
  GitHub — **does not** close the issue (a failed job leaves the ticket open, since
  "returns to the backlog" means work remains to be picked up, and GitHub's only closed
  states are `completed`/`not_planned`, neither of which fits "not done yet"); JIRA —
  attempt a transition to a status in the `to do` / `new` status category if reachable
  from the current status, else comment-only + warning.
- **Retried outbound calls are not deduplicated against a prior partial write.** If the
  tracker HTTP call itself succeeds but the process crashes before the follow-up
  `remote_revision` `Tx` commits, a subsequent `sync_ticket` call (Task 5) or another
  outbound trigger could post a second comment. This is a narrow, one-shot window (there
  is no automatic retry of the outbound write-back itself — only a *later, separate*
  MCP call could repeat it), named here rather than solved with idempotency keys neither
  GitHub's nor JIRA's comment APIs are documented to honor; see Risks & Open Questions.

**JIRA credential write-back (closes Task 2's deferred item).** Before constructing a
`JiraClient` call, if the stored refresh token has rotated (the client's token-refresh
path returns a new sealed pair per PR #20's `OAuthTokens`), the caller writes the new
`encrypted_credentials` back via `df_core::trackers::upsert_connection`, in the same
short follow-up `Tx` described above that writes `remote_revision` — this is the first
place in the codebase that both holds a live `Tx` and calls a JIRA API, so it is where
this plumbing was always going to land, per the Task 2 note.

**GitHub `installation_id` bridging (closes Task 2's other deferred item).**
`TrackerConnection.external_id: String` is parsed to `i64` before constructing a
`GithubAppClient` call (`external_id.parse::<i64>()`), failing loudly
(`Error::Invalid`, naming the connection and the unparseable value — never a silent
fallback) rather than panicking or defaulting, per Invariant 16. A row that reaches this
parse with a non-numeric `external_id` is a data-integrity bug (nothing else ever writes a
non-numeric value there), not a caller-facing input error, but it must still surface as a
named error rather than an `unwrap()`. For inbound resolution (§5a), `external_id` is
compared as a string throughout and never parsed — only the outbound GitHub API call
needs the typed `i64`.

## §5 What Task 1 does NOT wire up yet

No `df-trackers` dependency is added to `df-web` or `df-mcp` in this task — `df-core` gains
the schema, the typed accessors, and the promoted crypto primitive, but nothing calls the
tracker tables yet. This is deliberate: Task 1 is reviewable and mergeable in isolation
(schema + tests + a crate-internal crypto move, no behavior change to any running
surface), matching this repo's failing-test-first, one-task-at-a-time plan discipline.
Tasks 2+ (GitHub App client, JIRA client, webhook route, sync engine, MCP tools, console
UI) are recorded in `docs/plans/2026-09-03-df-trackers.md` and build on this foundation.

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

## §7 `link_ticket` / `sync_ticket` MCP tools (Task 5)

This section resolves the plan's Task 5 open design question ("confirm against Task 4's
revision-tracking decision whether this is per-job or per-repo-level binding reuse") and
the two tools' exact shapes.

**Resolution: per-job, not per-repo-binding reuse.** `tracker_bindings` (Task 1) already
answers "which tracker/project does this repo map to, and through which connection" —
that question is settled at the repo level and `link_ticket` does not touch it.
`jobs.tracker` / `jobs.ticket_ref` (added in Task 1, populated by `create_from_ticket` for
inbound-created jobs, left `NULL` by `add_job` for everything else) are what is actually
missing: a job created by hand (`add_job`, no `tracker` set even when a bare `ticket_ref`
was passed) or claimed before anyone thought to link it has no way to acquire a tracker
identity after the fact. `link_ticket` sets exactly those two columns on one job. §6's
outbound path already reads them (`checked first, before any tracker client is
constructed` — a job with neither is a no-op) and resolves *which connection* to use via
the existing repo-level `resolve_binding(tx, repo_id, provider)` — `link_ticket` supplies
the other half that path needs and changes nothing about how the connection is resolved.

**`link_ticket(job, tracker, ticketRef)`.** New `df-core` function
`jobs::link_ticket(tx, id, tracker, ticket_ref) -> Result<Job>`:

```sql
UPDATE jobs SET tracker = $3, ticket_ref = $4 WHERE org_id = $1 AND id = $2 RETURNING …
```

- `ticket_ref` must not be blank (trimmed), same guard `create_from_ticket` already
  applies to `title` — `Error::Invalid` if empty.
- **Also clears `remote_revision` to `NULL` in the same UPDATE.** A job's `remote_revision`
  is loop-safety state for §6's inbound stale/echo guard, scoped to *whichever ticket the
  job currently points at*. `link_ticket` can retarget an already-linked job (relink from
  ticket A to ticket B, or correct a wrong tracker), and leaving A's `remote_revision` in
  place would make the first genuine webhook for B look like an old echo and get silently
  dropped — the guard would be protecting against the wrong remote object. Since
  `link_ticket` is an explicit, infrequent admin-style action, unconditionally clearing it
  (even when tracker/ticket_ref happen not to change) is simpler than diffing old vs. new
  and costs nothing real.
- **Reuses `0015_jobs_ticket_ref_uniqueness.sql`'s existing partial unique index**
  (`(org_id, repo_id, tracker, ticket_ref) WHERE ticket_ref IS NOT NULL AND status IN
  ('pending','in-progress')`) rather than adding a second one — the index is table-wide,
  so any writer that would create two non-terminal jobs pointing at the same ticket in the
  same repo hits it, `link_ticket` included. Unlike `create_from_ticket`'s handling of the
  same index (a race between two *webhook deliveries* creating the same job, silently
  converged on since both callers meant the same thing), a `link_ticket` call hitting this
  index means a caller explicitly asked to attach a ticket another live job already owns —
  a genuine conflict the caller needs to know about, not a race to paper over. Caught via
  the same `SAVEPOINT`/`ROLLBACK TO SAVEPOINT` pattern `create_from_ticket` already uses
  (required because Postgres aborts the whole transaction on any statement error), but the
  recovery here is a new `Error::TicketAlreadyLinked { ticket_ref: String, job: JobId }`
  naming the job that already holds it, not a silent hand-back of that job — an agent
  linking a ticket does not mean the same thing as a webhook re-delivering an event it
  already applied. `code()` → `"ticket_already_linked"`; message: `"ticket {ticket_ref}
  is already linked to job {job} — unlink it there first, or use a different ticket_ref"`;
  `retriable()` → `false` (identical to `RemoteTaken`/`DependencyCycle`: retrying the same
  call cannot succeed, the caller's request itself needs to change).
- No format validation on `ticket_ref` at this layer — `df-core` stays provider-agnostic
  (matching `add_job`'s own "recorded, not resolved" framing for the same field).
  Provider-specific grammar is `df-trackers::jira::validate_jira_issue_key`'s job, applied
  at the point an outbound call actually needs the string as a URL path segment; a
  malformed ref surfaces there, on the first `sync_ticket`/queue-transition write-back that
  touches it, exactly the way an unresolvable repo surfaces at `resolve_repo`, not earlier.
- No repo/binding check: `link_ticket` does not require `tracker_bindings` to already have
  a binding for the job's `(repo_id, tracker)` pair. A job can be linked to a ticket before
  an admin finishes configuring the repo's binding — the outbound path already tolerates a
  missing/inactive binding as a no-op (§6, "a job can exist and be worked without its
  ticket ever being reachable"), and `link_ticket` does not need to duplicate that check to
  stay consistent with it.

**`sync_ticket(job)`.** Forces the same outbound write-back §6's `claim_jobs`/
`complete_job`/`fail_job` perform automatically after their own transition, but derived
from the job's **current** status rather than a status change that just happened — the
tool exists for exactly the cases §6 names as not self-healing: the retry window after a
tracker outage (§6's outbound guard section), and the moment right after `link_ticket`
where nothing has been posted to the ticket yet because no transition has fired since the
link was made.

- Requires `job.tracker` and `job.ticket_ref` both set — `Error::Invalid` naming the job
  and pointing at `link_ticket` if not (an LLM caller's next action, per Invariant 16).
- Maps the job's current `Status` to the same `JobTransition` §6's outbound path already
  knows how to render:
  - `InProgress` → `JobTransition::Claimed`, detail = `job.claimed_by_label`.
  - `Completed` → `JobTransition::Completed`, detail = `job.result`.
  - `Failed` → `JobTransition::Failed`, detail = `job.error`.
  - `Pending` → `Error::Invalid` — nothing has happened to this job yet, so there is no
    transition to (re-)announce; `link_ticket` alone does not imply a "claimed" comment
    that nobody actually claimed.
- **Reuses `sync_job_after_transition`'s binding-resolution and per-provider outbound
  helpers (`sync_github_job`/`sync_jira_job`), but not its "no binding" no-op and not its
  fire-and-forget wrapper.** §6's `sync_job_after_transition` returns `Ok(())` — a silent
  success — when the repo has no `tracker_bindings` row for the provider, when the binding
  has `connection_id IS NULL` (configured but not yet activated), or (the broken-invariant
  case, log-and-return) when the connection row is missing. That silence is correct for
  the automatic post-transition callers: the queue transition already happened and is the
  billed action, and there being nothing to sync to yet is not a failure of *that*. It is
  wrong for `sync_ticket`: an agent that explicitly asks "sync this ticket now" and gets a
  quiet `{"job": …}` back with nothing having happened cannot tell success from a no-op —
  exactly the ambiguity this tool exists to remove. Implementation therefore factors the
  shared resolution step (`resolve_binding` + `get_connection`) out of
  `sync_job_after_transition` into its own helper returning
  `Result<Option<(TrackerBinding, TrackerConnection)>, String>`; `sync_job_after_transition`
  keeps mapping `None` to `Ok(())` exactly as today, while `sync_ticket` maps `None` to a
  new `Error::Invalid` naming the missing piece ("this repo has no active {provider}
  binding — configure one before calling sync_ticket"). The per-provider network calls and
  their write-back (`sync_github_job`/`sync_jira_job`, and the `set_remote_revision` /
  `upsert_connection` write-back already inside them) are unchanged and shared by both
  paths — only the "is there anywhere to sync to" branch and the top-level error handling
  differ.
- **Surfaces a tracker/network failure as the tool's own error, unlike the fire-and-forget
  wrapper.** §6's automatic write-back after `claim_jobs`/`complete_job`/`fail_job` must
  never fail the tool call, because the queue transition it follows already succeeded and
  is the billed action — a tracker outage cannot be allowed to look like the claim itself
  failed. `sync_ticket` has no such transition to protect: talking to the tracker *is* the
  entire point of the call, so `sync_github_job`/`sync_jira_job` returning `Err` propagates
  as the tool's `McpResult` error (the wrapped HTTP/API error text) rather than being
  logged and swallowed. An agent calling `sync_ticket` needs to know whether the sync it
  explicitly asked for actually happened.
- **Billing is the second documented exception to "charge before the work," alongside
  `watch`.** For every other mutating tool in this file, `self.charge(&mut tx, ...)` is
  billable because the primary action *is* the Postgres write inside that same `tx` — the
  charge and the write commit or roll back together. `sync_ticket`'s primary action is the
  outbound HTTP call, which (per §6, "an external HTTP call has no place holding a Postgres
  transaction open") cannot happen inside that transaction, so whether the call is billable
  is only known *after* it returns. Charging up front — as an earlier draft of this section
  proposed — would bill a call whose tracker request then fails, which the metering
  invariant forbids ("a failed call is never billed"). `sync_ticket` therefore reads the
  job in a short, uncharged `Tx` (mirroring the read-then-act shape `resolve_binding`
  already uses), makes the outbound call, and — only on success — writes back
  `remote_revision`/rotated credentials in its own short `Tx`, which commits
  unconditionally before charging is even attempted. Charging then runs in a second,
  separate short `Tx`. This refines an earlier draft of this section, which proposed
  charging inside the same `Tx` as the write-back: the outbound call has, by this point,
  already happened and cannot be un-made, so a quota refusal at charge time must not be
  able to roll back the loop-safety state (`remote_revision`) that a subsequent retry
  depends on to avoid re-posting to the tracker a second time. A failed outbound call
  still returns its error with nothing written and nothing charged; a quota refusal after
  a successful outbound call returns its own error with nothing charged, but the
  write-back survives. `watch`'s "meters in its own short transaction" is the established
  precedent for a tool whose charge cannot live in the initiating transaction.
- Returns `{"job": …}` (`out::JobOut`) re-read after the write-back `Tx` commits (or the
  original read if the call resulted in a no-op that still counts as success — there is no
  such path once the binding-check above turns "nothing to sync to" into an error, so in
  practice this is always the post-write-back row) — never the pre-sync snapshot, so the
  returned `remote_revision` reflects what was actually written, not stale state.

**Scope and billing.** Both tools require the `trackers` scope — already declared in
`df_auth::oauth::KNOWN_SCOPES` and described on the consent screen
(`df-web/src/oauth.rs`: `"trackers" => "Link jobs to issues in JIRA or GitHub"`) since an
earlier task anticipated it; `df-mcp`'s `scope` module gains
`pub const TRACKERS: &str = "trackers";` to match. Both are already predeclared in
`df_billing::classify::BILLABLE` (Milestone 2 was priced ahead of being built, per that
module's own comment) — this task's only billing-table change is removing the "unbuilt"
carve-out in `classify::exhaustive_over` and its test's `built()` helper, now that both
tools exist on the router and the exemption is no longer needed.

**Cross-org negative test.** `link_ticket` is a new tenant-scoped write on the existing
`jobs` table (no new table, no new RLS policy needed — `jobs` is already governed by
`jobs_tenant_isolation`), but per Invariant 1 it still needs its own cross-org negative
test: attempting `link_ticket` against another org's job id must be refused/invisible,
exactly like the existing `create_from_ticket`/`update_from_ticket`/`close_from_ticket`/
`set_remote_revision` cross-org coverage in `crates/df-core/tests/isolation.rs`.

## Risks & Open Questions

- The existing free-form `repos.tracker_binding` `jsonb` column (Milestone 1) and the new
  structured `tracker_bindings` table now both describe "what tracker a repo maps to".
  Reconciling them (migrating the free-form column's data into the new table, or
  deprecating the column) is deferred to a later Milestone 2 task once the sync engine
  exists to consume the structured table — doing it in Task 1 would be a breaking change
  to a public-ish field with no consumer yet to validate against.
- One-connection-per-provider-per-org is a v1 simplification. If a customer legitimately
  needs two GitHub App installations in one org (e.g. two different GitHub orgs), this
  schema needs revisiting. Not raised in the design doc or the task brief; flagged here
  rather than solved speculatively.
- **The JIRA webhook path has a timing side-channel that reveals whether a `?site=<cloud-id>`
  is registered, independent of the shared secret.** An unregistered site returns 404 after
  a single indexed `SELECT` against `tracker_connection_index`; a registered site with the
  wrong secret additionally opens a `Tx` and decrypts the stored secret before returning the
  same 404 — strictly more work, and therefore measurably slower. Both responses are
  byte-identical (status + body), so this is a timing-only signal, not a content leak.
  Accepted rather than engineered around, for the same reason GitHub's installation id is
  treated as non-sensitive elsewhere in this design: a JIRA cloud site id is not a secret an
  org relies on for access control (the shared secret is), and confirming a site is
  *registered to some org on the platform* — without learning which org, or gaining any
  access — is a materially smaller leak than the credential-enumeration oracles this
  codebase actively defends against elsewhere (`CLAUDE.md`'s account-enumeration and
  redirect-URI-matching sections). Revisit only if a future threat model treats "is this
  JIRA site connected to dark-factory" as itself sensitive.
- **An inbound event whose payload omits a revision timestamp is applied unconditionally
  rather than rejected or held.** GitHub's `issues`/`issue_comment` payloads and JIRA
  Automation's issue payloads have always carried `updated_at`/`fields.updated` in
  practice, so this is a defensive fallback for a shape not yet observed, not a known gap.
  If a provider payload shape without it is ever seen, `jobs.remote_revision` is left at
  its last known-good value rather than cleared (§6) — revisit only if loop reports
  surface in practice, rather than adding speculative dedup machinery now for a payload
  shape neither provider currently sends.
