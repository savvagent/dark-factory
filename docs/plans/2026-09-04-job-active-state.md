# Job `active` state — implementation plan

**Spec:** `docs/specs/2026-09-04-job-active-state-design.md` — read it first. This plan
implements it exactly.

## Goal

Add `active` as a fourth, non-terminal `job_status` value an agent moves a claimed job
into via a new `activate_job` MCP tool, so a console or API client can tell a stalled
claim apart from real progress. `completed`/`failed` remain the only terminal states;
`complete_job`/`fail_job` accept a job in either `in-progress` or `active`.

## Status — 2026-09-04

Not started. All four tasks below are ⬜.

## Global Constraints

- No AI self-attribution in commits, code comments, or docs.
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `df-core` — `df-mcp`/`df-web` call `df-core` functions,
  never issue their own queries.
- Migrations are forward-only: `0003_jobs.sql` and `0015_jobs_ticket_ref_uniqueness.sql`
  are never edited. This plan's one migration (`0016_job_active_status.sql`) is additive.
- Tests need a real Postgres: `podman compose up -d` (Postgres 16 on host port 15433) and
  a `.env` with `DATABASE_URL` (`cp .env.example .env` if not already present).
- `activate_job` (Task 2) must be added to `df-billing::classify`'s `BILLABLE` list in the
  same commit that adds the tool, or `every_tool_has_a_price` (`crates/df-mcp/tests/tools.rs`)
  fails.
- This is an additive, non-breaking change (existing enum values, tools, routes, and
  response shapes are unchanged; only new ones are added) — no entry needed in
  `docs/clients/matrix.md`.
- No tenant table is added or touched — `jobs` already has tenant isolation; this plan
  changes what values one of its columns may hold, not its RLS shape.
- Console stays read-only over the queue — Task 3 touches only `GET` routes and their
  schemas.

## File Structure

| File | Responsibility |
|---|---|
| Create. `crates/df-core/migrations/0016_job_active_status.sql` | Adds `'active'` to `job_status`; widens `jobs_org_repo_tracker_ticket_open_idx` to include it. |
| Modify. `crates/df-core/src/jobs.rs` | `Status::Active`; `activate_job`; `finalize`/`close_from_ticket`/`repend_job` messaging; `get_live_job_by_ticket_for_repo`; `Stats.active` + query; lifecycle doc comment. |
| Modify. `crates/df-core/tests/queue.rs` | Cover the new transition (`activate_job`), the widened `finalize` starting states, and the `stats` counter. |
| Modify. `crates/df-core/tests/jobs.rs` | Cover the widened `close_from_ticket` starting states and the ticket-uniqueness regression (`link_ticket` against an `active` live holder). |
| Modify. `crates/df-mcp/src/tools/jobs.rs` | New `activate_job` tool; `sync_ticket`'s `Status` match; tool descriptions naming `active`; lifecycle doc comment. |
| Modify. `crates/df-mcp/tests/tools.rs` | Add `activate_job` to the advertised-surface list; end-to-end tests for the new tool and the widened `complete_job`/`fail_job` starting states. |
| Modify. `crates/df-billing/src/classify.rs` | Add `"activate_job"` to `BILLABLE`. |
| Modify. `crates/df-web/src/routes/jobs.rs` | `ListJobsQuery.status` doc comment. |
| Modify. `crates/df-web/src/openapi.rs` | Job status `enum`; `QueueStats` schema `active` property; a unit test asserting both. |
| Modify. `crates/df-web/tests/console.rs` | A job moved to `active` round-trips through the queue list and stats endpoints. |
| Modify. `web/src/lib/types.ts` | `JobStatus`, `QueueStats.active`. |
| Modify. `web/src/lib/components/StatusPill.svelte` | `active` tone (`--color-accent`). |
| Modify. `web/src/routes/o/[org]/queue/+page.svelte` | `STATUSES` list. |
| Modify. `web/src/routes/o/[org]/+page.svelte` | Overview "Active" stat tile. |
| Modify. `docs/specs/2026-09-01-dark-factory-design.md` | Job-model lifecycle line. |

## Task Order & Rationale

1. **`df-core`** first — the migration and `Status`/`Stats`/transition changes are the
   foundation everything else compiles against.
2. **`df-mcp` + `df-billing`** next — the new tool and its price, which only exist once
   `df-core` exposes `activate_job`.
3. **`df-web`** third — read-only surface reflecting the now-existing status value; has
   no dependency on Task 2's tool, only Task 1's `Status`/`Stats`, but ordered after the
   MCP surface since that is the primary interface per the design's own framing.
4. **`web/`** last — the console UI is the outermost layer and only needs the API shape
   Task 3 already exposes.

## Task 1 — `df-core`: the `active` status ⬜

**Files:** `crates/df-core/migrations/0016_job_active_status.sql`,
`crates/df-core/src/jobs.rs`, `crates/df-core/tests/queue.rs`,
`crates/df-core/tests/jobs.rs`

**Interfaces:**
- Produces: `Status::Active`; `Tx::activate_job(&mut self, id: &JobId) -> Result<Job>`;
  `Stats.active: i64`.
- Consumes: existing `Error::WrongStatus`, `Error::JobNotFound`, `Error::TicketAlreadyLinked`,
  `JOB_COLS`.

Steps:

- [ ] Write the failing tests first in `crates/df-core/tests/queue.rs`:
  - `activating_a_claimed_job_moves_it_to_active`: add a job, `claim_jobs` it, call
    `tx.activate_job(&j.id)`, assert `status == Status::Active` and
    `!status.is_terminal()`.
  - `activating_a_job_that_was_never_claimed_is_refused`: add a job (still `pending`),
    call `activate_job`, assert `err.code() == "wrong_status"`.
  - `completing_or_failing_an_active_job_still_works`: claim, activate, then
    `complete_job` — assert `status == Status::Completed`. Separately (new job),
    claim, activate, then `fail_job` — assert `status == Status::Failed`. Also assert
    the existing direct `in-progress → completed` path (no `activate_job` call, as
    `lifecycle_pending_to_completed` already does) still works unchanged, so the new
    state is additive rather than a new mandatory gate.
  - Extend `stats_counts_blocked_separately` (or add a sibling test right after it) to
    activate one job and assert `stats.active == 1` alongside the existing `pending`/
    `in_progress`/`completed`/`failed`/`blocked`/`total` assertions.
  - Run `cargo test -p df-core --test queue` — expect compile failures (`Status::Active`,
    `activate_job` don't exist yet) or the new assertions failing.
- [ ] Write the failing tests first in `crates/df-core/tests/jobs.rs` (this file, not
      `queue.rs`, is where `close_from_ticket` and the ticket-uniqueness/live-holder tests
      already live — see `close_from_ticket_allows_pending_and_in_progress` and
      `link_ticket_conflict_names_the_live_holder_not_a_newer_terminal_job`):
  - `close_from_ticket_allows_active`: mirror
    `close_from_ticket_allows_pending_and_in_progress`'s setup, but claim the job, call
    `tx.activate_job`, then `close_from_ticket(&job.id, Status::Completed, ...)` from
    `Active` and assert it succeeds.
  - `link_ticket_on_a_ticket_an_active_job_holds_returns_ticket_already_linked`: mirror
    `link_ticket_on_a_ticket_another_live_job_holds_returns_ticket_already_linked`'s setup
    exactly, but claim and `activate_job` the holder before the second job attempts
    `link_ticket` against the same `ticket_ref`. Assert `Error::TicketAlreadyLinked` names
    the holder — this is the ticket-dedupe regression the spec critique surfaced: an
    `active` job must still read as "live" to `get_live_job_by_ticket_for_repo`.
  - Run `cargo test -p df-core --test jobs` — expect compile/assertion failures until
    Task 1's `jobs.rs` changes land.
- [ ] Create `crates/df-core/migrations/0016_job_active_status.sql` exactly as specified
      in the spec's §1 (adds `'active'` to `job_status`; drops and recreates
      `jobs_org_repo_tracker_ticket_open_idx` to include `'active'` in its `WHERE`
      clause).
- [ ] In `crates/df-core/src/jobs.rs`:
  - Add `Active` to `enum Status` between `InProgress` and `Completed`, with
    `#[sqlx(rename = "active")]`/`#[serde(rename = "active")]` if needed (kebab-case
    already lowercases `active` identically, so an explicit rename attribute is only
    needed if `serde(rename_all = "kebab-case")` would not already produce `"active"` —
    verify by checking the derived output matches `"active"` with no attribute first,
    add one only if it doesn't).
  - `as_str`: add `Status::Active => "active"`.
  - `FromStr`: add `"active" => Ok(Status::Active)`; update the error message to list all
    five values.
  - Add `Tx::activate_job` per spec §2, placed near `finalize`.
  - `finalize`: change the guard to
    `!matches!(current, Status::InProgress | Status::Active)` and the `WrongStatus.expected`
    string to `"in-progress or active"`.
  - `close_from_ticket`: change the guard to
    `!matches!(current, Status::Pending | Status::InProgress | Status::Active)` and the
    `expected` string to `"pending, in-progress, or active"`.
  - `repend_job`: update the `WrongStatus.expected` string to
    `"completed, failed, in-progress, or active"` (logic unchanged).
  - `get_live_job_by_ticket_for_repo`: widen `status IN ('pending', 'in-progress')` to
    `status IN ('pending', 'in-progress', 'active')`.
  - `Stats`: add `pub active: i64` between `in_progress` and `completed`.
  - `stats()`: add `COUNT(*) FILTER (WHERE status = 'active') AS active` to the query,
    in the corresponding position.
  - Update the module doc comment (line 4) from
    `` `pending → in-progress → completed | failed`, with `repend` returning a `` to
    reflect the new `active` refinement (e.g. mention `in-progress` covers both "claimed"
    and its `active` refinement).
- [ ] Run `cargo test -p df-core --test queue` and `cargo test -p df-core --test jobs` —
      all pass.
- [ ] Run `cargo test -p df-core --test isolation` — unaffected, confirm still green (no
      tenant-table shape changed).
- [ ] `cargo fmt --all && git add -A && git commit -m "df-core: add active job status"`

## Task 2 — `df-mcp` + `df-billing`: `activate_job` tool ⬜

**Files:** `crates/df-mcp/src/tools/jobs.rs`, `crates/df-mcp/tests/tools.rs`,
`crates/df-billing/src/classify.rs`

**Interfaces:**
- Produces: MCP tool `activate_job` (input: `JobArgs { job }`, output: `out::JobOut`).
- Consumes: `Tx::activate_job` (Task 1), `Factory::charge`, `Factory::tx`, `JobTransition`
  (unchanged — `activate_job` performs no tracker sync).

Steps:

- [ ] Write the failing tests first in `crates/df-mcp/tests/tools.rs`:
  - Add `"activate_job"` to `the_advertised_surface_is_exactly_what_the_design_specifies`'s
    `expected` list (Jobs section, after `"claim_jobs"` or alongside `"complete_job"`).
    Running `cargo test -p df-mcp --test tools the_advertised_surface` now fails (tool
    doesn't exist yet).
  - `activating_a_claimed_job_marks_it_active_and_completion_still_works`: add a job,
    `claim_jobs`, call the `activate_job` tool, assert the returned job's `"status"` is
    `"active"`; then `complete_job` it and assert `"status"` is `"completed"` — proving
    the widened `finalize` guard end-to-end.
  - `activate_job_on_an_unclaimed_job_is_refused`: add a job (pending), call
    `activate_job`, assert the MCP error names `wrong_status`/mentions `"in-progress"`.
  - Extend/confirm an existing direct-completion test (e.g.
    `ticketless_job_transitions_still_succeed` or `the_full_loop_from_remote_url_to_completed_job`)
    still passes unchanged, proving `activate_job` is optional.
  - Run `cargo test -p df-mcp --test tools` — expect compile failure (`activate_job` tool
    doesn't exist) and `every_tool_has_a_price` failure once it does exist but is
    unclassified.
- [ ] In `crates/df-mcp/src/tools/jobs.rs`:
  - Add the `activate_job` tool per spec §3 (description text, `JobArgs` input,
    `out::JobOut` output, `self.charge(&mut tx, &caller, "activate_job")`, no
    `sync_jobs_after_transition` call).
  - Extend `sync_ticket`'s `match job.status` with
    `Status::Active => (JobTransition::Claimed, job.claimed_by_label.clone())`.
  - Update `ListJobsArgs.status`'s doc comment, `list_jobs`'s tool description, and
    `stats`'s tool description to list `active` among the valid values. Also update
    `sync_ticket`'s tool description (currently enumerates
    `"(in-progress, completed, or failed)"` and
    `"to be in-progress, completed, or failed"`) to include `active`, since
    `Status::Active` is now a valid starting state for it (§3's match extension).
  - Update the module doc comment (near line 3) the same way as Task 1's `jobs.rs`
    change.
- [ ] In `crates/df-billing/src/classify.rs`: add `"activate_job"` to the `BILLABLE`
      array (near `"claim_jobs"`/`"complete_job"`/`"fail_job"`), and add `"activate_job"`
      to the `for tool in [...]` billable-assertion list in the module's own tests.
- [ ] Run `cargo test -p df-mcp --test tools` and `cargo test -p df-billing` — all pass,
      including `every_tool_has_a_price`.
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --all && git add -A && git commit -m "df-mcp: add activate_job tool"`

## Task 3 — `df-web`: expose `active` on the read-only queue surface ⬜

**Files:** `crates/df-web/src/routes/jobs.rs`, `crates/df-web/src/openapi.rs`,
`crates/df-web/tests/console.rs`

**Interfaces:**
- Consumes: `Status::from_str` (Task 1, already handles `"active"`), `Tx::stats` (Task 1,
  already returns `active`).
- No new routes — existing `GET` handlers pass the wider `Status`/`Stats` through
  unchanged.

Steps:

- [ ] Write the failing test first in `crates/df-web/src/openapi.rs`'s existing
      `#[cfg(test)] mod tests` block (alongside its other schema assertions): a test
      asserting the job status `enum` array in the `Job` schema contains `"active"`, and
      that the `QueueStats` schema's `properties`/`required` both contain `"active"`.
      Run `cargo test -p df-web openapi::tests` — fails until the schema is updated below.
- [ ] Write the failing test first in `crates/df-web/tests/console.rs`:
      `an_active_job_is_visible_in_the_queue_and_its_stats`: reuse the `enqueue` helper to
      seed jobs, move one to `active` via `df-core` directly (claim then `activate_job`,
      the same way other console tests seed job state through `df-core` rather than the
      console API), then assert `GET /api/orgs/{org}/jobs?status=active` returns exactly
      that job and `GET /api/orgs/{org}/jobs/stats` reports `"active": 1`. Run
      `cargo test -p df-web --test console an_active_job` — this may already pass once
      Task 1 lands (nothing in `df-web`'s Rust code needs to change for `Status`/`Stats`
      round-tripping — only the OpenAPI *document*, a separately maintained JSON literal,
      needs the update below); that is fine, the assertion is the regression guard going
      forward, and the openapi.rs unit test above is what actually catches schema drift.
- [ ] Update `crates/df-web/src/routes/jobs.rs`'s `ListJobsQuery.status` doc comment to
      `` `pending` | `in-progress` | `active` | `completed` | `failed` ``.
- [ ] Update `crates/df-web/src/openapi.rs`: add `"active"` to the job status `enum`
      array (in the `Job` schema, ~line 474); add an `"active": { "type": "integer" }`
      property to the `QueueStats` schema and to its `required` array (~line 523/529).
- [ ] Run `cargo test -p df-web --test console` and the `openapi` unit tests — all pass.
- [ ] `cargo fmt --all && git add -A && git commit -m "df-web: document the active job status"`

## Task 4 — `web/`: console UI ⬜

**Files:** `web/src/lib/types.ts`, `web/src/lib/components/StatusPill.svelte`,
`web/src/routes/o/[org]/queue/+page.svelte`, `web/src/routes/o/[org]/+page.svelte`

**Interfaces:**
- Consumes: the `active` value now present in API responses from Task 3.

Steps:

- [ ] `web/src/lib/types.ts`: add `'active'` to the `JobStatus` union (between
      `'in-progress'` and `'completed'`); add `active: number;` to the `QueueStats`
      interface (between `inProgress` and `completed`).
- [ ] `web/src/lib/components/StatusPill.svelte`: add
      `active: 'border-accent/50 bg-accent/10 text-accent'` to `tones`, matching the
      existing entries' shape.
- [ ] `web/src/routes/o/[org]/queue/+page.svelte`: add `'active'` to the `STATUSES` array
      between `'in-progress'` and `'completed'`.
- [ ] `web/src/routes/o/[org]/+page.svelte`: add an "Active" tile to the overview stat
      tiles, next to `'In progress'`, reading `stats.active` (reuse the same tile shape
      the existing `'In progress'` entry uses; pick a `tone` distinct from `text-busy`,
      e.g. `text-accent`, matching the `StatusPill` choice above).
- [ ] Run `cd web && npm run check` — passes (type additions satisfy `svelte-check`).
- [ ] Run `cd web && npm run lint` — passes.
- [ ] Run `cd web && npm test` — passes unchanged (no Worker routing change).
- [ ] Run `cd web && npm run build` — succeeds.
- [ ] No Rust changed in this task, so `cargo fmt --all` is a no-op here — skip it. If
      the edits need reformatting, run `cd web && npm run format` (prettier --write),
      matching `web/package.json`'s actual script (not `npm run lint -- --write`, which
      is not how this repo's formatter is invoked), then:
      `git add -A && git commit -m "web: show the active job status in the console"`

## Task 5 — Docs ⬜

**Files:** `docs/specs/2026-09-01-dark-factory-design.md`

Steps:

- [ ] Update the job-model lifecycle line (currently
      `` | Job model + lifecycle (`pending → in-progress → completed/failed`) | The TUI, the PTY host, and every hook | ``)
      to mention `active` as a refinement of `in-progress`, without implying it is a
      fifth terminal or mandatory state.
- [ ] `git add -A && git commit -m "docs: note the active job status in the design doc"`

## Final gate (all tasks)

- [ ] `cargo test --workspace`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --all --check`
- [ ] `cd web && npm run check && npm run lint && npm test && npm run build`
