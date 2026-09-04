# sync_ticket quota pre-check — implementation plan

**Spec:** `docs/specs/2026-09-04-sync-ticket-quota-precheck-design.md` — read it first. This
plan implements it exactly.

## Goal

Give `sync_ticket` a read-only quota check ahead of its outbound tracker call, so an org on
a hard-stop plan already over its bucket is refused with `quota_exceeded` before any
GitHub/JIRA write-back is attempted, without disturbing the existing loop-safety guarantee
(the write-back still commits before the post-hoc `charge` call).

## Status — 2026-09-04

Task 1 implemented in this PR (savvagent/dark-factory#30) and covered by
`sync_ticket_refuses_before_the_outbound_call_when_over_budget` in
`crates/df-mcp/tests/tools.rs`. Marked complete once that PR merges.

## Global Constraints

- No AI self-attribution in commits, code comments, or docs.
- Run `cargo fmt --all` before every Rust commit.
- Every SQL statement lives in `df-core` — this task adds none, and must not add any
  outside it.
- Tests need a real Postgres: `podman compose up -d` (already running in this environment,
  host port 15433) and a `.env` with `DATABASE_URL` (already present).
- Metering runs inside the tool's own transaction, before the work, for every tool except
  `watch` and `sync_ticket` — this task does not change that shape, only closes a gap in
  `sync_ticket`'s existing exception.
- No new MCP tool, no new `df-billing::classify` entry needed — `sync_ticket` is already
  classified as billable; this is a change to *when* it can be refused, not *what* it is.
- This is a non-breaking behavioral tightening: the tool's caller-visible contract only
  gets stricter about when it refuses, using the existing `quota_exceeded` error shape.

## File Structure

| File | Responsibility |
|---|---|
| Modify. `crates/df-billing/src/meter.rs` | Extract `charge`'s enforcement check into `check_quota`; add `Meter::would_refuse`. |
| Modify. `crates/df-mcp/src/server.rs` | Add `Factory::would_refuse`, mapping `BillingError` to `ErrorData`. |
| Modify. `crates/df-mcp/src/tools/jobs.rs` | Call `self.would_refuse` in `sync_ticket`'s initial job-fetch transaction, before either tracker arm. |
| Modify. `crates/df-mcp/tests/tools.rs` | Add a regression test proving the pre-check runs before the outbound call. |

## Task Order & Rationale

One task: the change is small and cuts across three files in a single call chain
(`Meter` → `Factory` → `sync_ticket`), so splitting it into sub-tasks would only fragment
a change that has to land together to compile and to be meaningfully tested.

## Task 1 — Quota pre-check for `sync_ticket` ✅

**Files:** `crates/df-billing/src/meter.rs`, `crates/df-mcp/src/server.rs`,
`crates/df-mcp/src/tools/jobs.rs`, `crates/df-mcp/tests/tools.rs`

**Interfaces:**
- Produces: `Meter::would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<()>` (new,
  public, in `df-billing`); `Factory::would_refuse(&self, tx: &mut Tx<'_>, tool: &str) ->
  Result<(), ErrorData>` (new, public, in `df-mcp`).
- Consumes: existing `Tx::plan_limits`, `Tx::current_usage` (`df-core`), existing
  `BillingError::QuotaExceeded`, existing `error::from_billing`.

Steps:

- [ ] Write the failing regression test first in `crates/df-mcp/tests/tools.rs`, near the
      existing `sync_ticket_*` group (after `sync_ticket_reports_an_outbound_failure_as_retriable`,
      which this test's setup mirrors). Name it
      `sync_ticket_refuses_before_the_outbound_call_when_over_budget`. Shape:
      - `env_metered(pool, Meter::new(true, UPGRADE_URL))` (enforcement on).
      - Set up the DB `tracker_connections`/`tracker_bindings` rows exactly like
        `sync_ticket_reports_an_outbound_failure_as_retriable` does (`upsert_connection` +
        `upsert_binding` for `Provider::Github`) — a binding IS present, but the server's
        GitHub App itself remains unconfigured, so an outbound call, if attempted, fails
        with `tracker_sync_failed`. Then add a job, `link_ticket` it to GitHub with a
        well-formed `ticket_ref` (e.g. `"acme/api#21"`), and `claim_jobs` it.
      - Seed `org_period_usage` to exactly `included_ops` (500 for the Free plan, matching
        `enforcement_stops_work_but_never_reads`'s seed) via the same raw `sqlx::query`
        upsert those existing tests use.
      - Call `sync_ticket`. Assert `code_of(&e) == "quota_exceeded"` — **not**
        `"tracker_sync_failed"` — and that `e.data["retriable"] == false`, matching every
        other `quota_exceeded` refusal.
      - Run: `cargo test -p df-mcp --test tools sync_ticket_refuses_before_the_outbound_call_when_over_budget`
        — expect a compile error (`would_refuse` does not exist yet) or a runtime failure
        (`tracker_sync_failed` instead of `quota_exceeded`) if a compile-only stub is used;
        either way, confirm it fails for the right reason before implementing.
- [ ] In `crates/df-billing/src/meter.rs`, extract the `if self.enforce && class.is_billable()
      && limits.hard_stop { ... }` block from `charge` into a private
      `async fn check_quota(&self, tx: &mut Tx<'_>, tool: &str, class: Class, limits: &PlanLimits) -> Result<()>`,
      call it from `charge` in place of the inline block, and add the public
      `pub async fn would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<()>` per the
      spec's §1, with the doc comment from the spec verbatim (adapted to the final code).
      `PlanLimits` needs `.clone()` where `check_quota` borrows it but `QuotaExceeded` needs
      an owned `display_name` — confirm `PlanLimits` derives `Clone` (it does; verified in
      `crates/df-core/src/usage.rs`) so `limits.display_name.clone()` compiles.
- [ ] Run `cargo test -p df-billing` — confirm existing unit tests (`remaining_never_goes_negative`,
      `the_warning_starts_at_four_fifths`, `an_empty_bucket_is_always_over`) still pass
      unchanged; the refactor must not touch `remaining`/`warning`.
- [ ] In `crates/df-mcp/src/server.rs`, add `pub async fn would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<(), ErrorData>`
      next to `charge`, with the doc comment from the spec's §2, mapping errors via
      `error::from_billing` exactly like `charge` does.
- [ ] In `crates/df-mcp/src/tools/jobs.rs`'s `sync_ticket`, add
      `self.would_refuse(&mut tx, "sync_ticket").await?;` after `tx.get_job(...)` and before
      `tx.commit()` in the initial transaction block. Update the existing comment above that
      block (currently explaining only why charging doesn't happen there) to also explain the
      pre-check, per the spec's §3.
- [ ] Run `cargo test -p df-mcp --test tools sync_ticket` — confirm the new test passes and
      every existing `sync_ticket_*` test still passes (in particular
      `sync_ticket_reports_an_outbound_failure_as_retriable`, which must still return
      `tracker_sync_failed` when enforcement is off/default, proving the pre-check only
      changes behavior when enforcement + hard-stop + over-budget all hold).
- [ ] Run `cargo test -p df-mcp --test tools` (whole suite) to catch any interaction with
      `enforcement_stops_work_but_never_reads`, `the_last_included_operation_is_allowed`,
      `with_enforcement_off_an_over_budget_org_keeps_working`, `usage_is_counted_per_org` —
      all must still pass unchanged.
- [ ] Run `cargo test --workspace` — full regression.
- [ ] Run `cargo clippy --all-targets -- -D warnings`.
- [ ] No tenant table, no MCP tool, no migration, no console route touched — nothing further
      required for tenant isolation or metering classification (`every_tool_has_a_price` /
      `exhaustive_over` in `df-billing` need no update, since `sync_ticket`'s classification
      is unchanged).
- [ ] Out-of-band artifacts: none touched (`Dockerfile`, `fly.toml`, `web/`, `web/worker/`,
      `crates/df-core/migrations/`, `.github/workflows/`, `.env.example` all untouched) —
      vacuously satisfied, state so explicitly in the PR body.
- [ ] Format and commit: `cargo fmt --all`, then
      `git commit -m "df-billing: add read-only quota pre-check for sync_ticket"` (one
      commit covering `meter.rs`, `server.rs`, `jobs.rs`, and the new test — they must land
      together to compile).
