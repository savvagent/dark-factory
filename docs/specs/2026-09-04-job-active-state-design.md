# Job `active` state design

> **Status:** IMPLEMENTED — adds an `active` job state between `in-progress` (claimed) and
> `completed`/`failed`, closing savvagent/dark-factory#13.

## Goal & Success Criteria

`in-progress` today means both "claimed" and "being worked on right now," and a console
or API client cannot tell those apart — a job an agent claimed and then abandoned looks
identical to one mid-flight. This adds `active` as a fourth, non-terminal `job_status`
value an agent moves into once it has actually started work, via a new MCP tool
(`activate_job`). It is a refinement of `in-progress` from a client's point of view: it
does not change what counts as terminal, and every place that today accepts
`in-progress` as "not finished yet" is extended to accept `active` too.

Success:

- `job_status` gains `'active'` via a new, additive migration — `0003_jobs.sql` is not
  edited.
- A claimed job can move `in-progress → active` via a new `activate_job` MCP tool, and
  `complete_job`/`fail_job` accept a job in either `in-progress` or `active` as their
  starting state (an agent does not have to call `activate_job` to finish work — it is
  an optional, additional signal, not a mandatory gate).
- `list_jobs`/`ready`/`blocked`/`stats` and the console's read-only queue views all
  recognize `active` as a status value; `stats` reports an `active` counter alongside
  `pending`/`inProgress`/`completed`/`failed`/`blocked`.
- `Status::is_terminal()` is unaffected — still only `completed`/`failed`.
- No existing test asserting `pending → in-progress → completed | failed` behavior
  regresses; new tests cover the `in-progress → active → completed|failed` path and the
  direct `in-progress → completed|failed` path (skipping `active`) side by side.
- `activate_job` is classified in `df-billing::classify` (billable, alongside
  `claim_jobs`/`complete_job`/`fail_job`) — `every_tool_has_a_price` must not fail.

## Public interface note

This change is **additive, not breaking**, per Non-Negotiable Rule 6: `job_status` gains
a new enum value (existing values `pending`/`in-progress`/`completed`/`failed` and their
meanings are unchanged), and `activate_job` is a new MCP tool alongside the existing
ones — no tool, field, route, or column is renamed or removed, and no result envelope's
shape changes. `Stats` gains a new field (`active`), which is additive to an existing
response shape. No version bump is required.

## Scope

**In:**

- New migration `crates/df-core/migrations/0016_job_active_status.sql` adding `'active'`
  to the `job_status` enum.
- `crates/df-core/src/jobs.rs`: `Status::Active` variant, `as_str`/`FromStr`, a new
  `Tx::activate_job` transition method, `finalize`/`close_from_ticket` accepting `Active`
  as a valid starting state, `Stats.active` counter and its query, and widening
  `get_live_job_by_ticket_for_repo`'s open-status filter to include `active`.
- A second migration, `crates/df-core/migrations/0017_job_active_ticket_index.sql`,
  recreates `jobs_org_repo_tracker_ticket_open_idx` (`0015_jobs_ticket_ref_uniqueness.sql`)
  to treat `active` as an open status, so the ticket-dedupe guarantee does not regress for
  a job that has moved past `in-progress`. This must be a separate migration file from
  0016 — see §1 for why.
- `crates/df-mcp/src/tools/jobs.rs`: new `activate_job` tool (`ActivateJobArgs` reuses the
  existing single-`job` shape), updated tool descriptions listing `active` among valid
  states, and the `sync_ticket` status match extended to handle `Status::Active`.
- `crates/df-billing/src/classify.rs`: classify `activate_job` as billable.
- `crates/df-web`: `openapi.rs` status enum + `QueueStats` schema, `routes/jobs.rs` doc
  comments listing valid `status` query values.
- `web/`: `JobStatus` type, `StatusPill` tone, queue page's status filter list, and the
  overview page's stat tiles gain an `active` entry.
- Tests: `crates/df-core/tests/queue.rs` and `tests/jobs.rs`, `crates/df-mcp/tests/tools.rs`,
  `crates/df-web/tests/console.rs`, and `web/` type-checking (`npm run check`) all cover
  the new state.
- Doc updates: the four lifecycle-diagram comments found in `jobs.rs` (both crates) and
  `docs/specs/2026-09-01-dark-factory-design.md`'s job-model line.

**Out:**

- Any change to `is_terminal()`'s definition or to what closes a job — `completed` and
  `failed` remain the only terminal states, unchanged.
- Automatic fallback from `active` back to `in-progress` on inactivity (a
  heartbeat/staleness computation). The issue raises this as an open question; per
  constraint 2 (substrate, not workflow), computing "has this gone quiet" is exactly the
  kind of methodology decision that belongs in a customer's own skill watching `watch`/
  `list_jobs`, not in the server. `active` is only ever set or left alone by an explicit
  agent call — never demoted by the server on its own.
- A tracker-side "active" transition concept. GitHub Issues and JIRA have no native
  status matching "claimed and now actively worked" beyond what `Claimed` already covers;
  `sync_ticket`/the automatic write-back after a transition map `Status::Active` to the
  same `JobTransition::Claimed` behavior `Status::InProgress` already gets (see §3).
  Introducing a new tracker-side transition is a larger, separate piece of work with no
  concrete external target today.
- Enforcing that only the claiming agent may call `activate_job` — no existing job
  transition (`complete_job`, `fail_job`) enforces claimant identity today (any caller
  with `JOBS_WRITE` in the org may act on any job), and `activate_job` matches that
  existing, deliberate lack of enforcement rather than introducing a new rule
  asymmetrically for one tool.
- Any change to the console's write surface. `activate_job` is an MCP tool only; the
  console queue view remains read-only, consistent with `the_queue_is_read_only_over_the_console`.

## §1 — Migration

`crates/df-core/migrations/0016_job_active_status.sql`:

```sql
-- Adds 'active' to job_status: an agent-signaled refinement of 'in-progress'
-- meaning "claimed AND actively being worked on right now," distinct from a
-- claim that may have stalled. Additive only — 0003_jobs.sql is never edited;
-- enum values can only be appended, and ordering here does not matter because
-- nothing compares job_status by its enum ordinal.
ALTER TYPE job_status ADD VALUE 'active';
```

`crates/df-core/migrations/0017_job_active_ticket_index.sql`:

```sql
-- 0015_jobs_ticket_ref_uniqueness.sql's partial unique index enforces at most
-- one *open* (non-terminal) job per (repo, tracker, ticket_ref), scoped to
-- 'pending'/'in-progress'. An active job is still open — it must keep
-- blocking a second job for the same ticket, or a webhook race could create
-- a duplicate for a ticket whose job has moved to 'active'. 0015 is never
-- edited (it is already applied); the index is dropped and recreated here
-- with the same shape, widened to include 'active'.
DROP INDEX jobs_org_repo_tracker_ticket_open_idx;
CREATE UNIQUE INDEX jobs_org_repo_tracker_ticket_open_idx
    ON jobs (org_id, repo_id, tracker, ticket_ref)
    WHERE ticket_ref IS NOT NULL AND status IN ('pending', 'in-progress', 'active');
```

These must be two separate migration files, not one. Postgres refuses to let a new enum
value be *used* — even as a string literal in a partial index's `WHERE` clause, not just
a cast — in the same transaction that added it: `unsafe use of new value "active" of
enum type job_status ... New enum values must be committed before they can be used.`
sqlx's migrator runs each migration file in its own transaction, so 0016 must commit
before 0017's index rebuild can reference `'active'`. This was verified empirically
against the same Postgres 16 the test suite runs against (matching the version pinned in
`podman-compose.yml`) — combining them into one file fails every test that exercises a
fresh migration run.

`get_live_job_by_ticket_for_repo` (`crates/df-core/src/jobs.rs`) backs the conflict
lookup callers make after losing a race against this index — its own `status IN
('pending', 'in-progress')` filter is widened to `('pending', 'in-progress', 'active')`
in §2, so it keeps agreeing with what the index now considers "open."

## §2 — `crates/df-core/src/jobs.rs`

```rust
pub enum Status {
    Pending,
    #[sqlx(rename = "in-progress")]
    #[serde(rename = "in-progress")]
    InProgress,
    Active,
    Completed,
    Failed,
}
```

`as_str`/`FromStr` gain the `"active"` arm; `FromStr`'s error message lists all five
values. `is_terminal` is untouched — `matches!(self, Status::Completed | Status::Failed)`
already excludes `Active` by construction.

New transition, mirroring the shape of `finalize` but for the one non-terminal-to-non-
terminal move:

```rust
/// Confirm a claimed job is being actively worked on, not merely claimed.
/// This is a refinement signal only — `complete_job`/`fail_job` accept a job
/// in `in-progress` OR `active` as their starting state, so calling this is
/// optional, never a gate an agent must pass through to finish work.
pub async fn activate_job(&mut self, id: &JobId) -> Result<Job> {
    let org = self.org();
    let current: Option<Status> =
        sqlx::query_scalar("SELECT status FROM jobs WHERE org_id = $1 AND id = $2 FOR UPDATE")
            .bind(org)
            .bind(id)
            .fetch_optional(self.conn())
            .await?;

    let current = current.ok_or_else(|| Error::JobNotFound(id.clone()))?;
    if current != Status::InProgress {
        return Err(Error::WrongStatus {
            job: id.clone(),
            actual: current.as_str().to_string(),
            expected: "in-progress".into(),
        });
    }

    let job = sqlx::query_as(&format!(
        "UPDATE jobs SET status = 'active' WHERE org_id = $1 AND id = $2 RETURNING {JOB_COLS}"
    ))
    .bind(org)
    .bind(id)
    .fetch_one(self.conn())
    .await?;

    Ok(job)
}
```

`finalize` (backs `complete_job`/`fail_job`) changes its guard from
`current != Status::InProgress` to
`!matches!(current, Status::InProgress | Status::Active)`, and its `WrongStatus.expected`
string from `"in-progress"` to `"in-progress or active"`.

`close_from_ticket`'s guard changes from
`!matches!(current, Status::Pending | Status::InProgress)` to
`!matches!(current, Status::Pending | Status::InProgress | Status::Active)`, and its
`expected` string from `"pending or in-progress"` to `"pending, in-progress, or active"`.

`repend_job`'s existing guard (`current == Status::Pending` is the only rejected state)
needs no logic change — `Active` already falls into "anything but pending" — but its
`WrongStatus.expected` string ("completed, failed, or in-progress") is updated to
"completed, failed, in-progress, or active" so the error names every valid alternative.

`Stats` gains an `active: i64` field (placed after `in_progress`, before `completed`, to
read as the lifecycle order), and `stats()`'s query gains
`COUNT(*) FILTER (WHERE status = 'active') AS active`.

`get_live_job_by_ticket_for_repo`'s `status IN ('pending', 'in-progress')` filter is
widened to `status IN ('pending', 'in-progress', 'active')`, matching the widened index
from §1 — an active job is still the "live" holder of its ticket_ref for conflict
lookups after a webhook race.

`activate_job` does not touch `started_at` — it was already set at claim time
(`claim_jobs` sets it to `now()`) and keeps meaning "when this job was first claimed,"
not "when work actually began." `activate_job` adds no new timestamp column; the moment
of activation is observable only via the status change itself (and, if a caller wants a
record of it, `watch`/change notifications already fire on every `UPDATE jobs`).

## §3 — `crates/df-mcp/src/tools/jobs.rs`

New tool, same argument shape as `repend_job` (`JobArgs { job }`):

```rust
#[tool(
    name = "activate_job",
    description = "Confirm a job you claimed is being actively worked on right now, not \
                   just claimed. This is what lets a console or API client tell a stalled \
                   claim apart from real progress — call it once you actually start, not \
                   at claim time. Optional: complete_job and fail_job both accept a job \
                   that never called this."
)]
pub async fn activate_job(
    &self,
    Extension(parts): Extension<http::request::Parts>,
    Parameters(args): Parameters<JobArgs>,
) -> Result<Json<out::JobOut>, ErrorData> {
    let caller = self.caller(&parts)?;
    caller.require_scope(scope::JOBS_WRITE).mcp()?;

    let mut tx = self.tx(&caller).await?;
    self.charge(&mut tx, &caller, "activate_job").await?;
    let job = tx.activate_job(&JobId::from(args.job)).await.mcp()?;
    tx.commit().await.mcp()?;

    Ok(Json(out::JobOut { job }))
}
```

No tracker write-back call after this transition — see Scope/Out above; `activate_job`
does not call `sync_jobs_after_transition`.

`sync_ticket`'s exhaustive match over `job.status` (today: `Pending` errors, `InProgress`
→ `Claimed`, `Completed` → `Completed`, `Failed` → `Failed`) gains:

```rust
Status::Active => (JobTransition::Claimed, job.claimed_by_label.clone()),
```

identical to the `InProgress` arm — from a tracker's point of view "claimed" and "claimed
and actively working" are the same external state.

`ListJobsArgs.status`'s doc comment, `list_jobs`'s tool description, and `stats`'s tool
description are updated to list `active` among the valid values (currently: "pending",
"in-progress", "completed" or "failed").

## §4 — `crates/df-billing/src/classify.rs`

`activate_job` is added to the `BILLABLE` list alongside `claim_jobs`/`complete_job`/
`fail_job` — it is a state-changing write on the same footing as those three, not a read.
`every_tool_has_a_price`/`exhaustive_over` enforce this is not forgotten.

## §5 — `crates/df-web`

- `routes/jobs.rs`'s `ListJobsQuery.status` doc comment: `` `pending` | `in-progress` |
  `active` | `completed` | `failed` ``.
- `openapi.rs`: the job status `enum` array gains `"active"`; the `QueueStats` schema
  gains an `"active": { "type": "integer" }` property, added to `required` alongside the
  others.

No route changes — the console stays read-only over the queue; `active` is just another
value the existing `GET` endpoints pass through.

## §6 — `web/`

- `src/lib/types.ts`: `JobStatus` gains `'active'`; the `QueueStats` interface gains
  `active: number`.
- `src/lib/components/StatusPill.svelte`: `tones` gains an `active` entry using the
  existing `--color-accent` theme token (`web/src/app.css`) — `border-accent/50
  bg-accent/10 text-accent`. `accent` is otherwise unused for status pills (`ok`/`busy`/
  `warn`/`bad` are the status palette; `accent` is the app's one blue), so it is visually
  distinct from `busy` (`in-progress`'s yellow) and every other tone without adding a new
  color to the design system.
- `src/routes/o/[org]/queue/+page.svelte`: `STATUSES` gains `'active'` between
  `'in-progress'` and `'completed'`.
- `src/routes/o/[org]/+page.svelte`: overview stat tiles gain an "Active" tile next to
  "In progress", reading `stats.active`.

## §7 — Testing

- `crates/df-core/tests/queue.rs`: a test claiming a job, calling `activate_job`, and
  asserting the status is `active` and `is_terminal()` is still false; a test that
  `complete_job`/`fail_job` succeed directly from `in-progress` (unchanged path) and also
  from `active` (new path); a test that `activate_job` on a `pending` or already-
  `completed`/`failed`/`active` job returns `WrongStatus` naming `"in-progress"` as
  expected; a `stats()` test asserting the new `active` counter.
- `crates/df-core/tests/jobs.rs` (where `close_from_ticket`'s and the ticket-uniqueness/
  live-holder tests already live): `close_from_ticket` succeeding from `Active`; and a
  test that a ticket-linked job moved to `active` still reads as the *live* holder of its
  `ticket_ref` — `link_ticket` from a second job against the same ref returns
  `Error::TicketAlreadyLinked` naming the active job, proving
  `jobs_org_repo_tracker_ticket_open_idx` and `get_live_job_by_ticket_for_repo` both
  still treat `active` as open — the regression the spec critique surfaced.
- `crates/df-mcp/tests/tools.rs`: `activate_job` end-to-end (claim → activate → assert
  status), `activate_job` rejected on an unclaimed job, `complete_job`/`fail_job` still
  passing without a prior `activate_job` call, and `activate_job` appearing in the tool
  list with a description (existing `tests/tools.rs` assertion pattern). Billing test
  extended: `activate_job` added to the billable-tools list alongside `claim_jobs`.
- `crates/df-web/tests/console.rs`: a job moved to `active` (via `df-core` directly, the
  same way other console tests seed state) round-trips through `GET .../jobs` and
  `GET .../jobs/stats` with the new status/counter visible.
- `web/`: `npm run check` (the type additions must satisfy `svelte-check`), `npm run
  lint`, `npm test` — no Worker routing change, so `npm test`'s existing suite should
  pass unchanged; run it as the regression gate.

## Assumptions

- `activate_job` is a new MCP tool rather than an optional flag on `claim_jobs`, per the
  issue's own framing ("may reuse claim or need a distinct call") — claiming and starting
  work are observably different moments (a job can sit claimed-but-idle for a long time
  before work actually starts), so collapsing them into one call would reintroduce the
  exact ambiguity this change exists to remove. *Rationale: matches the issue's stated
  problem precisely.*
- `active` is purely agent-asserted, never computed from a heartbeat or elapsed time —
  see Scope/Out. *Rationale: constraint 2; stalled-claim detection is a policy decision
  belonging in a customer's skill.*
- No ownership check on `activate_job` (any caller with `JOBS_WRITE` may activate any
  job in the org) — matches `complete_job`/`fail_job`'s existing lack of a claimant check
  rather than introducing inconsistent enforcement for one tool. *Rationale: consistency
  over incidental hardening; a real ownership model is a larger, separate change
  touching all three tools together.*
- `Status::Active` maps to `JobTransition::Claimed` for tracker sync purposes — see
  Scope/Out. *Rationale: no tracker has a matching external state; treating it as
  "still claimed" from the tracker's point of view is accurate and avoids inventing a
  transition with no target.*

## Risks & Open Questions

- `ALTER TYPE ... ADD VALUE` inside a migration transaction is supported by the Postgres
  version this project targets (16), but if the CI Postgres image or the migrator's
  transaction handling disagrees, the migration would need splitting into its own
  non-transactional step. Flagged for verification during implementation (Task 1's
  first `cargo test` run against a real migrated database is the check).
- None beyond the migration-transaction check above — the `StatusPill` color question
  is resolved in §6 (`--color-accent`, already declared in `web/src/app.css`).
