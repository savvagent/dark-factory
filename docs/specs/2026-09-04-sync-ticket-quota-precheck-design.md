# sync_ticket quota pre-check design

> **Status:** IMPLEMENTED — the read-only quota check shipped in
> savvagent/dark-factory#30.
> **Depends on:** `docs/specs/2026-09-03-df-trackers-design.md` §7 (`link_ticket`/
> `sync_ticket`) — merged in PR #26.

## Goal & Success Criteria

`sync_ticket` (`crates/df-mcp/src/tools/jobs.rs`) charges for its call *after* the
outbound tracker write-back succeeds, not before — a deliberate exception to the rule
documented in `crates/df-billing/src/meter.rs` ("charging happens before the work, so a
refusal costs nothing"), because charging in the same transaction as the write-back would
let a quota refusal roll back loop-safety state (`remote_revision`, rotated JIRA
credentials) for a tracker call that already happened, making a caller's retry re-post to
the tracker a second time.

That trade-off has a gap: an org on a hard-stop plan that is already over its bucket still
gets its outbound tracker write posted — a real side effect (a GitHub/JIRA comment or
status transition) — before it finds out, via the later `charge` call, that the operation
was refused. `CLAUDE.md`'s metering section says enforcement "refuses only billable tools
on hard-stop plans" before the work; `sync_ticket` doesn't honor that for its actual side
effect.

Success:

- An org on a hard-stop plan already at or past its bucket gets `sync_ticket` refused
  with `quota_exceeded` *before* any outbound call to GitHub or JIRA is attempted — no
  network call is made, and no write-back happens.
- The existing loop-safety guarantee is unchanged: the write-back still commits in its own
  transaction before the post-hoc `charge` call, so a *legitimate* charge failure after a
  successful pre-check (a narrow race — the count changed in between) still cannot lose
  `remote_revision`/rotated-credential state.
- No other tool's charging path changes. `Meter::charge`'s existing behavior (record usage
  first, refuse before recording if enforcement + hard-stop + over budget) is untouched.
- A test proves the ordering: with enforcement on, a hard-stop plan, and the bucket
  exhausted, `sync_ticket` against a job whose binding has no working tracker App
  configured returns `quota_exceeded` — not `tracker_sync_failed`, which is what the same
  setup returns today and would still return if the outbound call ran before the check.

## Scope

**In:**
- A new read-only quota check on `Meter` (`crates/df-billing/src/meter.rs`) that mirrors
  `charge`'s enforcement/hard-stop/bucket logic but never calls `record_usage`.
- A thin wrapper on `Factory` (`crates/df-mcp/src/server.rs`) exposing that check, mapped
  to `ErrorData` the same way `charge` is.
- Calling that check from `sync_ticket` before the tracker-specific outbound call
  (`sync_github_job` / `sync_jira_job`), for both the GitHub and JIRA arms.
- A regression test in `crates/df-mcp/tests/tools.rs`.

**Out:**
- Changing the post-write-back `charge` call — it stays exactly as it is, for exactly the
  reason documented in `server.rs`.
- Changing `watch`'s metering shape, or any other tool's charge ordering.
- A new MCP tool, a new console route, a new migration, or any change to the tenant
  isolation surface — this touches no tenant table and adds no SQL. It belongs entirely
  inside the existing `Meter`/`Factory` charging path, which is exactly the kind of
  substrate-level correctness fix the three constraints in `CLAUDE.md` are silent on: it
  changes nothing about *how* work is specified or planned, only when an existing,
  documented enforcement rule takes effect for one tool.
- Retrying the outbound call automatically after a refusal, or surfacing a different error
  shape than the existing `quota_exceeded` (`BillingError::QuotaExceeded`) callers already
  handle from every other tool.

## §1 — `Meter::would_refuse`

Extract the enforcement/hard-stop/bucket check that already lives inside `charge` into a
private helper, and expose a second public entry point that runs the same check without
recording usage:

```rust
impl Meter {
    pub async fn charge(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<Charge> {
        let class = classify::classify(tool);
        let limits = tx.plan_limits().await?;
        self.check_quota(tx, tool, class, &limits).await?;
        let usage = tx.record_usage(Some(user), tool, class.is_billable()).await?;
        Ok(Charge::new(class, usage, limits))
    }

    /// Check whether a call would be refused, without recording any usage.
    ///
    /// Used by `sync_ticket` (`df_mcp::tools::jobs`), whose "work" is an
    /// outbound tracker write that has already happened by the time
    /// `charge` runs (see `Factory::charge`'s doc comment) — without this,
    /// an org on a hard-stop plan already over its bucket gets that write
    /// posted anyway, only to be told afterwards that it wasn't billed.
    /// This reads the same counters `charge` re-checks before recording
    /// usage, so the two can disagree only if the count changes in
    /// between — a narrow, acceptable race no different from any other
    /// check-then-act gap, and strictly better than not checking at all.
    pub async fn would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<()> {
        let class = classify::classify(tool);
        let limits = tx.plan_limits().await?;
        self.check_quota(tx, tool, class, &limits).await
    }

    async fn check_quota(
        &self,
        tx: &mut Tx<'_>,
        tool: &str,
        class: Class,
        limits: &PlanLimits,
    ) -> Result<()> {
        if self.enforce && class.is_billable() && limits.hard_stop {
            let before = tx.current_usage().await?;
            if before.billable_count >= limits.included_ops {
                return Err(BillingError::QuotaExceeded {
                    tool: tool.to_string(),
                    used: before.billable_count,
                    included: limits.included_ops,
                    plan: limits.display_name.clone(),
                    upgrade_url: self.upgrade_url.clone(),
                });
            }
        }
        Ok(())
    }
}
```

This is a pure refactor of `charge`'s existing logic plus one new sibling method — no
behavior change to `charge` itself, verified by the existing `meter.rs`/`tools.rs` test
suite passing unchanged.

## §2 — `Factory::would_refuse`

`crates/df-mcp/src/server.rs` gets a thin wrapper next to `charge`, mapping
`BillingError` to `ErrorData` the same way:

```rust
/// Read-only quota check for `sync_ticket`'s two-transaction charging shape
/// — see `charge`'s doc comment for why that tool cannot charge before its
/// outbound call the way every other tool does. This runs the same
/// enforcement/hard-stop/bucket check `charge` does, without recording
/// usage, so `sync_ticket` can refuse before making that call.
pub async fn would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<(), ErrorData> {
    self.meter
        .would_refuse(tx, tool)
        .await
        .map_err(|e| error::from_billing(&e))
}
```

## §3 — `sync_ticket`'s call site

In `crates/df-mcp/src/tools/jobs.rs`, the initial transaction that fetches the job (opened
before either tracker arm runs) also runs the pre-check, before it commits:

```rust
let mut tx = self.tx(&caller).await?;
let job = tx.get_job(&JobId::from(args.job)).await.mcp()?;
self.would_refuse(&mut tx, "sync_ticket").await?;
tx.commit().await.mcp()?;
```

This runs strictly before every other step in the tool — the tracker/ticket_ref presence
checks, `resolve_tracker_binding`, the ticket_ref format pre-validation, and both outbound
calls — so it satisfies "ahead of the outbound call" for both the GitHub and JIRA arms
with one call site rather than two. The existing doc comment on the initial `tx` block
(explaining why charging doesn't happen here) is updated to also explain the pre-check.

The post-write-back `charge` calls in both arms (today at `jobs.rs:1075` and `:1135`) are
untouched.

## §4 — Error shape

`would_refuse`'s failure is the same `BillingError::QuotaExceeded` → `quota_exceeded`
error code every other tool already returns when refused, mapped through the same
`error::from_billing`. A caller does not need to learn a new error shape for this one
tool.

No new `df-billing::classify` entry is needed: `sync_ticket` is already classified as
billable, and this change only adds a pre-check on that existing tool's path — it neither
adds a tool nor changes its class. Likewise, this is a non-breaking behavioral tightening
of an existing refusal path: the tool's caller-visible contract only gets stricter about
*when* it refuses, using the same `quota_exceeded` error shape every other tool already
returns.

## §5 — Testing

- `crates/df-billing/src/meter.rs`: no new unit tests needed beyond what's there — the
  quota arithmetic (`remaining`, `warning`) is unchanged, and `would_refuse`/`check_quota`
  share the exact logic `charge`'s existing tests already exercise indirectly through
  other tools. Add one focused unit-level assertion only if the refactor introduces a
  behavior seam the existing tests don't already cover (they do: `check_quota` is called
  identically from both paths).
- `crates/df-mcp/tests/tools.rs`: add a test alongside the existing `sync_ticket_*` group
  that reuses `sync_ticket_reports_an_outbound_failure_as_retriable`'s setup (a binding
  with no GitHub App configured, so an outbound call would fail with
  `tracker_sync_failed`), but with `Meter::new(true, UPGRADE_URL)` and the bucket
  exhausted via the same `org_period_usage` seed `enforcement_stops_work_but_never_reads`
  uses. Assert the error code is `quota_exceeded`, not `tracker_sync_failed` — proving the
  check ran *before* the outbound attempt, since the unconfigured App means the outbound
  call would otherwise be the first thing to fail.
- Confirm `enforcement_stops_work_but_never_reads`, `the_last_included_operation_is_allowed`,
  and `with_enforcement_off_an_over_budget_org_keeps_working` still pass unchanged — they
  exercise `charge` through other tools and must not regress from the `check_quota`
  extraction.

## Assumptions

- Placing the pre-check in the initial job-fetch transaction (before either tracker arm)
  rather than immediately before each tracker-specific call is equivalent for this
  guarantee: nothing between that point and the outbound call spends any part of the
  bucket, so "ahead of" holds for both arms from one call site. *Rationale: simpler, one
  call site instead of two, and the existing tx is already open and free to extend.*
- `would_refuse` takes `tool: &str` (matching `charge`'s signature) rather than being
  hardcoded to `"sync_ticket"`, so it is reusable if a future tool needs the same
  check-before-side-effect shape. *Rationale: mirrors `charge`'s own generality; costs
  nothing extra.*
- The narrow race (bucket crosses the line between the pre-check and the post-write-back
  `charge` call) is accepted as-is, per the issue's own framing — closing it fully would
  require holding a lock across the outbound network call, which is a worse trade-off than
  the race it would close. *Rationale: matches the issue's suggested fix exactly.*

## Risks & Open Questions

- None identified beyond the accepted race above.
