# Milestone 2 — df-trackers: GitHub App + JIRA two-way sync

**Spec:** [`docs/specs/2026-09-03-df-trackers-design.md`](../specs/2026-09-03-df-trackers-design.md)
— read it first. This plan implements it exactly.

Goal: turn `df-trackers` from a one-line stub into the two-way sync engine the design of
record describes — GitHub App + JIRA connections bound per org, tracker bindings per repo,
inbound webhook ingest that creates/updates jobs, outbound job-transition writeback, and
the `link_ticket`/`sync_ticket` MCP tools — while keeping every existing invariant
(tenant isolation's two guards, SQL confined to `df-core`, metering classification,
no AI attribution, forward-only migrations) intact.

## Status — 2026-09-03

Task 1 ✅ shipped (PR #18). Task 2 ✅ shipped (PR #20). Tasks 3–6 ⬜, not started.

## Global Constraints

- Every SQL statement lives in `df-core`. No query in `df-trackers`, `df-web`, or `df-mcp`.
- Every tenant-scoped function takes an explicit `OrgId` and threads it through `Tx`
  (Guard 1); every new tenant table gets a `<table>_tenant_isolation` FORCE RLS policy
  discovered by `Db::verify_tenant_isolation`'s naming convention (Guard 2), plus a
  cross-org negative test. Without the negative test the task is not done.
- Migrations are forward-only, one file per concern, in `crates/df-core/migrations/`.
  Never edit an applied migration — add a new one.
- Run `cargo fmt --all` before every Rust commit. No `unwrap()` outside tests. No AI
  self-attribution anywhere (commits, PR bodies, comments, docs).
- Coordination stays anchored on repos: a tracker binding always names a `repo_id`, never
  a standalone tracker entity with no repo. Any `DF_*` config this milestone adds
  (`DF_GITHUB_APP_ID`, `DF_GITHUB_APP_PRIVATE_KEY`, `DF_GITHUB_APP_WEBHOOK_SECRET`, later)
  must fail `Config::from_env` loudly on an unparseable value, never default silently.
- No credential is ever spent on a `GET` — the webhook route (Task 3) and any console
  binding action (Task 6) that consumes a one-time code must be a `POST`.
- Tests need `podman compose up -d` and a `.env` with `DATABASE_URL`
  (`cp .env.example .env`). `#[sqlx::test]` gives each test a fresh throwaway database;
  there are no database mocks.
- Any new MCP tool must be classified in `df-billing::classify` in the same task that adds
  it — `every_tool_has_a_price` fails otherwise.
- `df-mcp` tools: the org comes from the token, never a tool argument; results are a
  one-field object in `tools::out`; descriptions are written for an LLM caller with no docs.
- A breaking change to a public interface (MCP tool surface, console API, config surface,
  schema) must be named explicitly and flagged to the architect reviewer. None is planned
  in this file; if a task discovers it needs one, stop and update this plan and the spec
  first.

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `crates/df-core/migrations/0011_trackers.sql` | `tracker_connections`, `tracker_bindings`, RLS policies |
| **Create.** `crates/df-core/src/crypto.rs` | `Cipher`/`Sealed` (promoted from `df-auth`) |
| **Modify.** `crates/df-core/src/error.rs` | `Error::Config`, `Error::Crypto` variants |
| **Modify.** `crates/df-core/src/lib.rs` | `pub mod crypto;`, `pub mod trackers;` |
| **Create.** `crates/df-core/src/trackers.rs` | `TrackerConnection`/`TrackerBinding` rows + CRUD, sealed-value encoding |
| **Modify.** `crates/df-core/Cargo.toml` | add `aes-gcm`, `rand`, `subtle`, `base64` |
| **Modify.** `crates/df-core/tests/isolation.rs` | cross-org negative tests for both new tables |
| **Modify.** `crates/df-auth/src/crypto.rs` | delete `Cipher`/`Sealed` and their tests (moved) |
| **Modify.** `crates/df-auth/Cargo.toml` | drop now-unused crypto deps if nothing else in the crate uses them |
| **Modify.** `crates/df-server/src/main.rs`, `crates/df-server/src/lib.rs` | `df_auth::crypto::Cipher` → `df_core::crypto::Cipher` |
| **Modify.** `crates/df-web/src/state.rs`, `crates/df-web/src/lib.rs` | same import change |
| **Modify.** `crates/df-web/tests/common/mod.rs` | same import change (test helper) |
| **Create.** `crates/df-core/tests/trackers.rs` | integration tests for the new CRUD, mirroring `tests/queue.rs`'s shape |
| (Task 2+, not this plan revision's active task) `crates/df-trackers/src/github.rs` | GitHub App client |
| (Task 2+) `crates/df-trackers/src/jira.rs` | JIRA OAuth 3LO client |
| (Task 3+) `crates/df-trackers/src/webhook.rs` + `crates/df-web/src/...` | signature verification + `/webhooks/{provider}` route |
| (Task 4+) `crates/df-trackers/src/sync.rs` | inbound/outbound sync engine |
| (Task 5+) `crates/df-mcp/src/tools/trackers.rs`, `crates/df-billing/src/classify.rs` | `link_ticket`/`sync_ticket` tools + pricing |
| (Task 6+) `web/src/...` | console UI for binding a connection + per-repo tracker binding |

## Task Order & Rationale

Task 1 (schema + crypto) has no consumer and is reviewable/mergeable in total isolation —
it changes no running behavior. Tasks 2–3 (GitHub/JIRA clients, webhook route) each need
Task 1's schema to store what they mint. Task 4 (sync engine) needs 1–3. Task 5 (MCP
tools) needs the sync engine to have something to call and needs billing classified in the
same task per the Global Constraints. Task 6 (console UI) is last because it is the only
task with no test-suite gate beyond `npm run check`/`npm run lint`/`npm test`, and it reads
whatever the server-side tasks exposed.

---

## Task 1 — Schema foundation: `tracker_connections`, `tracker_bindings`, promoted crypto ✅ (PR #18)

**Files:** `crates/df-core/migrations/0011_trackers.sql`, `crates/df-core/src/crypto.rs`,
`crates/df-core/src/trackers.rs`, `crates/df-core/src/error.rs`, `crates/df-core/src/lib.rs`,
`crates/df-core/Cargo.toml`, `crates/df-core/tests/isolation.rs`,
`crates/df-core/tests/trackers.rs`, `crates/df-auth/src/crypto.rs`,
`crates/df-auth/Cargo.toml`, `crates/df-server/src/main.rs`, `crates/df-server/src/lib.rs`,
`crates/df-web/src/state.rs`, `crates/df-web/src/lib.rs`, `crates/df-web/tests/common/mod.rs`.

**Interfaces:** produces `df_core::trackers::{Provider, TrackerConnection, TrackerBinding,
upsert_connection, get_connection, delete_connection, upsert_binding, get_binding,
delete_binding, resolve_binding}` and `df_core::crypto::{Cipher, Sealed}` for later tasks
to consume. Consumes nothing new (no dependency edges added).

- [ ] Add `aes-gcm`, `rand`, `subtle`, `base64` to `crates/df-core/Cargo.toml` (all already
      workspace dependencies used by `df-auth`; just add the `df-core` `[dependencies]`
      entries).
- [ ] Move `Cipher`/`Sealed` (and their unit tests, unchanged) from
      `crates/df-auth/src/crypto.rs` to a new `crates/df-core/src/crypto.rs`. Add
      `pub mod crypto;` to `crates/df-core/src/lib.rs`.
- [ ] Add `Error::Config(String)` and `Error::Crypto(String)` to `crates/df-core/src/error.rs`
      (match the exact wording the moved tests assert: "DF_ENCRYPTION_KEY is not valid
      base64", "DF_ENCRYPTION_KEY must decode to 32 bytes, got {n}", "failed to seal
      secret", "stored nonce has the wrong length", "failed to open secret — wrong key or
      tampered ciphertext").
- [ ] Delete `Cipher`/`Sealed` and their tests from `crates/df-auth/src/crypto.rs`; keep
      `generate`, `hash`, `verify`, `prefix::*` (these are what `df-web/src/routes/auth.rs`
      and `df-web/src/routes/orgs.rs` actually call — confirmed unaffected).
- [ ] Update every `df_auth::crypto::Cipher` reference to `df_core::crypto::Cipher`:
      `crates/df-server/src/main.rs`, `crates/df-server/src/lib.rs`,
      `crates/df-web/src/state.rs` (`use df_auth::crypto::Cipher` → `use df_core::crypto::Cipher`),
      `crates/df-web/src/lib.rs`, `crates/df-web/tests/common/mod.rs`.
- [ ] Run `cargo build --workspace` and `cargo test -p df-auth` — confirm the move compiles
      clean and every remaining `df-auth` test (the ones exercising `generate`/`hash`/
      `verify`/`prefix`) still passes. This proves the move is behavior-neutral before
      any new tracker code is written.
- [ ] Write a failing test first in a new `crates/df-core/tests/trackers.rs` (mirrors the
      `#[sqlx::test]` shape in `crates/df-core/tests/queue.rs` and the shared setup in
      `crates/df-core/tests/common/mod.rs` — there is no `tests/repos.rs`; `queue.rs` is
      the closest existing example of per-org CRUD + cross-org assertions in this crate).
      Cover: upsert/get/delete on both tables, the `ON CONFLICT (org_id, provider) DO
      UPDATE` replace-on-rebind behavior, and `connection_id` becoming `NULL` when a
      connection is deleted out from under a binding. Run it — confirm it fails to compile
      (the module and migration do not exist yet).
- [ ] Write `crates/df-core/migrations/0011_trackers.sql`: `tracker_provider` enum
      (`github`, `jira`), `tracker_connections` (`org_id NOT NULL`, `provider`,
      `external_id`, nullable `encrypted_credentials`, nullable `encrypted_webhook_secret`,
      `UNIQUE (org_id, provider)`), `tracker_bindings` (`org_id NOT NULL`, `repo_id`,
      `connection_id` nullable `ON DELETE SET NULL`, `provider`, `external_ref`,
      `UNIQUE (repo_id, provider)`), indexes on `org_id` and `connection_id` for bindings.
      Exact column list and comments per spec §1. In the same migration, add a `DO $$ …
      $$` block exactly matching `0007_rls.sql`'s existing loop shape (confirmed):
      `ALTER TABLE <t> ENABLE ROW LEVEL SECURITY`, `ALTER TABLE <t> FORCE ROW LEVEL
      SECURITY`, then
      `CREATE POLICY <t>_tenant_isolation ON <t> USING (org_id = current_org()) WITH CHECK (org_id = current_org())`
      for `tracker_connections` and `tracker_bindings` — reusing the `current_org()`
      function `0007_rls.sql` already defined; do not redefine it.
- [ ] Write `crates/df-core/src/trackers.rs`: `Provider` enum (`sqlx::Type` →
      `tracker_provider`), `TrackerConnection`/`TrackerBinding` structs
      (`FromRow`/`Serialize`/`JsonSchema`, matching `repos.rs`'s derive list), private
      `encode_sealed`/`decode_sealed` helpers implementing the canonical
      `base64(nonce || ciphertext)` encoding from spec §4, and CRUD functions
      (`upsert_connection`, `get_connection`, `delete_connection`, `upsert_binding`,
      `get_binding`, `delete_binding`, `resolve_binding`) each taking `&mut Tx` and an
      explicit `org_id` bind on every statement. Add `pub mod trackers;` to
      `crates/df-core/src/lib.rs`. Run `crates/df-core/tests/trackers.rs` again — confirm
      it now compiles and passes (migrations auto-apply against the throwaway
      `#[sqlx::test]` database; no manual migration step needed for this check).
- [ ] Write a failing cross-org negative test in `crates/df-core/tests/isolation.rs` for
      `tracker_connections` and `tracker_bindings`, following the file's existing pattern
      for another tenant table: create a row under org A inside a normal `Tx`, then open a
      second transaction with `SET LOCAL ROLE df_app; SET LOCAL app.org_id = '<org B>'`
      and issue an unscoped `SELECT`/`UPDATE`/`DELETE` against the same table, asserting
      zero rows visible/mutable. Temporarily comment out the two new `CREATE POLICY`
      statements in `0011_trackers.sql` and confirm the new test fails (proving it isn't a
      false positive), then restore the policies and confirm it passes.
- [ ] Confirm the production migration path once: `cp .env.example .env` if not already
      present, `podman compose up -d`, then `cargo run -p df-server` briefly and check the
      logs for `db.migrate().await` (the exact call in `crates/df-server/src/main.rs`)
      completing without error — this is the only place migrations run outside the test
      harness. `Ctrl-C` once it logs a successful bind.
- [ ] `cargo test -p df-core --test isolation`, `cargo test -p df-core --test trackers`,
      `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`,
      `cargo fmt --all --check`. All green before commit.
- [ ] `cargo fmt --all` (writes formatting), then commit: `df-core: tracker_connections, tracker_bindings, and the promoted crypto primitive`.

---

## Task 2 — GitHub App + JIRA OAuth clients ✅ (PR #20)

**Remaining (deferred to the sync-engine task, since `df-trackers` performs no SQL):**
persisting a rotated JIRA refresh token back into `tracker_connections.encrypted_credentials`,
and bridging `TrackerConnection.external_id: String` to `GithubAppClient`'s
`installation_id: i64`. PR #20 shipped the client-side halves of both (`OAuthTokens`
returns the rotated pair and exposes `seal_refresh_token`/`open_refresh_token`; the GitHub
client takes a typed `i64`) — the write-back and the parse-and-fail-loudly bridge belong to
whichever task first constructs a `Tx` around a live call (Task 4 or 5).

**Files:** `crates/df-trackers/src/github.rs`, `crates/df-trackers/src/jira.rs`,
`crates/df-trackers/src/lib.rs`, `crates/df-trackers/Cargo.toml` (add `jsonwebtoken` for
GitHub App JWT signing if not already present — check first), `crates/df-server` config
(`DF_GITHUB_APP_ID`, `DF_GITHUB_APP_PRIVATE_KEY`, `DF_GITHUB_APP_WEBHOOK_SECRET` — new env
vars, additive, documented in `.env.example` with the *why*).

- [x] GitHub: mint a JWT from the App id + private key (RS256, 10-minute expiry per
      GitHub's own requirement), exchange it for a short-lived installation access token
      per `external_id` (the installation id stored in `tracker_connections`), cache
      in-memory with the token's own expiry (installation tokens are ~1 hour).
- [x] GitHub: issue/comment API calls (`POST /repos/{owner}/{repo}/issues/{n}/comments`,
      label read, issue state PATCH) using the minted token.
- [x] JIRA: authorization-code exchange for a refresh token pair, encrypted via
      `df_core::crypto::Cipher` into `tracker_connections.encrypted_credentials`;
      refresh-token rotation on expiry, writing the new sealed value back. *(The
      exchange, rotation, and seal/open helpers shipped in PR #20; the actual write-back
      into `tracker_connections` needs a `Tx`, which `df-trackers` cannot construct — see
      Remaining above.)*
- [x] JIRA: issue API calls (comment, transition) using the site id (`external_id`) and
      the rotating access token.
- [x] Recorded-fixture tests (no live network) for both clients, per the design doc's own
      testing guidance for `df-trackers`.
- [x] `cargo test -p df-trackers`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`.
- [x] Commit.

## Task 3 — Webhook ingest ⬜

**Spec:** see §5a ("Webhook org resolution") in `docs/specs/2026-09-03-df-trackers-design.md`
— read it first. It resolves a real gap the earlier checklist glossed over: every
`df-core::trackers` accessor takes a `Tx` pinned to an already-known `OrgId`, but a webhook
arrives with only a provider-native id (GitHub installation id / JIRA site id) — the org
is exactly what's being looked up, so the normal RLS-scoped path cannot answer it (an
unscoped query against `tracker_connections`, which is `FORCE ROW LEVEL SECURITY`, returns
zero rows with no `app.org_id` set). §5a adds a narrow, secret-free reverse-index table
(`tracker_connection_index`, deliberately outside RLS, mirroring `access_tokens`'s
bootstrap exemption) and one new unscoped resolver function,
`df_core::trackers::resolve_connection_org`, that the webhook route calls once per request
before opening a normal `Tx` for everything else.

**Files:** `crates/df-core/migrations/0012_tracker_connection_index.sql` (new, additive),
`crates/df-core/src/trackers.rs` (add `tracker_connection_index` maintenance to
`upsert_connection`/`delete_connection`, add `resolve_connection_org`),
`crates/df-trackers/src/webhook.rs` (new — signature verification + event parsing),
a new route added to `df-web`'s `catalog.rs` (`/webhooks/{provider}`, unauthenticated by
design — verified by signature instead of a session/token) plus its handler module.

- [ ] Migration: add `tracker_connection_index` (provider, external_id, org_id,
      connection_id — PK `(provider, external_id)`), no RLS enabled, never added to any
      `tenant_tables` array. Confirm a fresh cluster applies cleanly
      (`podman compose down -v && podman compose up -d`, `cargo test -p df-core`).
- [ ] `df-core::trackers::upsert_connection` additionally upserts the matching
      `tracker_connection_index` row in the same `Tx`; `delete_connection` additionally
      deletes it. Both stay atomic with the real write — no separate transaction.
- [ ] `df-core::trackers::resolve_connection_org(db: &Db, provider, external_id) ->
      Result<Option<OrgId>>` — deliberately takes `&Db`, not `&mut Tx`, and reads only
      `tracker_connection_index`. Doc-comment states it is the one place a tracker table is
      read without an org already pinned, and that a second such accessor must not be added.
- [ ] Cross-org test (not an `rls_scopes_*`-style unscoped-SQL test — there is no policy to
      probe, by design): upsert connections for two different orgs, assert
      `resolve_connection_org` returns each org's own id and never the other's. Comment the
      test explaining why this isn't an RLS test, so a future reader doesn't mistake the
      absence of one for an oversight.
- [ ] GitHub: HMAC-SHA256 verification of `X-Hub-Signature-256` against
      `DF_GITHUB_APP_WEBHOOK_SECRET`, constant-time compare.
- [ ] JIRA: shared-secret verification per Automation webhook's configured header/query
      parameter (finalize exact mechanism against JIRA's current docs at implementation
      time — the spec left this as a Task-3 decision).
- [ ] Parse `issues`/`issue_comment` (GitHub) and Automation payloads (JIRA) into a
      provider-neutral event type `df-trackers` exposes.
- [ ] Webhook route: verify signature → parse event → extract provider id (installation id
      / site id) → `resolve_connection_org` → open a `Tx` for that `OrgId` → `get_connection`
      / `resolve_binding` for everything else. An id that resolves to no org is a `404`-shaped
      response with no detail (never confirm/deny which ids are registered to an attacker
      probing the endpoint), logged for operator visibility.
- [ ] `catalog.rs` entry with summary/description; confirm route is reachable and add to
      `the_whole_router_assembles`-style startup coverage if `df-server` needs updating.
- [ ] Recorded-fixture tests for signature verification (valid, tampered, replayed) and
      event parsing.
- [ ] `cargo test -p df-core`, `cargo test -p df-trackers`, `cargo test -p df-web`, clippy, fmt.
- [ ] Commit.

## Task 4 — Two-way sync engine ⬜

**Files:** `crates/df-trackers/src/sync.rs`.

- [ ] Inbound: a labelled issue creates or updates a job through `df-core::jobs`; closing
      the ticket cancels/completes the job.
- [ ] Outbound: hook `claim_jobs`/`complete_job`/`fail_job` (called from `df-mcp`) to post
      a comment and transition the linked ticket, using the tracker client from Task 2.
- [ ] Loop-safety: every write records the resulting remote revision on the job (a new
      `jobs` column or a side table — decide and record here before implementing, per the
      Global Constraints' breaking-change rule if it touches the `jobs` table's public
      shape); an inbound event carrying a revision this server just wrote is dropped.
- [ ] Cross-org negative test if any new tenant table is added for revision tracking.
- [ ] `cargo test --workspace`, clippy, fmt.
- [ ] Commit.

## Task 5 — `link_ticket` / `sync_ticket` MCP tools ⬜

**Files:** `crates/df-mcp/src/tools/trackers.rs`, `crates/df-mcp/src/tools/mod.rs`,
`crates/df-mcp/src/tools/out.rs`, `crates/df-billing/src/classify.rs`.

- [ ] `link_ticket(job_id, provider, external_ref)` — creates/updates a `tracker_bindings`-
      backed link on the job (design detail: confirm against Task 4's revision-tracking
      decision whether this is per-job or per-repo-level binding reuse).
- [ ] `sync_ticket(job_id)` — forces an immediate outbound sync.
- [ ] Both added to `df-billing::classify::BILLABLE` (per the design doc's own
      classification table, already predeclared) — `every_tool_has_a_price` must pass.
- [ ] Tool descriptions written for an LLM caller with no docs, matching the rest of
      `df-mcp`'s tool surface.
- [ ] `cargo test -p df-mcp --test tools`, `cargo test -p df-billing`, clippy, fmt.
- [ ] Commit.

## Task 6 — Console UI: tracker connections + bindings ⬜

**Files:** `web/src/routes/o/[org]/settings/...` (org-level connection binding),
`web/src/routes/o/[org]/repos/[repo]/...` (repo-level tracker binding), `df-web`'s
`catalog.rs` if new read/write REST routes are needed (the console API stays read-only
over the *queue* specifically — a tracker-connection admin action is not a queue write,
same as existing repo-registration console flows).

- [ ] Bind GitHub App installation / JIRA site at the org level (admin-only, `OrgCtx::require_admin`).
- [ ] Set a repo's tracker binding (project key / owner-repo) from the repo settings page.
- [ ] `npm run check && npm run lint && npm test && npm run build`.
- [ ] Commit.

**Out-of-band reminders for whichever task lands last:** confirm `DF_GITHUB_APP_PRIVATE_KEY`
and any other new `DF_*` vars are documented in `.env.example` with the *why*, and that
`Config::from_env` errors (never silently defaults) on an unparseable value.
