//! Charging a tool call against an org's bucket.
//!
//! ## Why this happens *before* the work rather than after
//!
//! [`Meter::charge`] is called immediately after the tool opens its
//! transaction, before it does anything. That looks wrong for the rule "a
//! failed call is not billed" — and it is exactly what makes the rule true.
//! The usage row is written inside the tool's own transaction, so a tool that
//! fails and returns an error rolls the transaction back and takes the meter
//! with it. Charging afterwards would need a second transaction, which can fail
//! on its own, be retried, or be forgotten — all three of which produce a bill
//! that disagrees with what happened.
//!
//! Doing it first also puts the quota check where a refusal costs nothing: the
//! caller is told no before any work is performed, rather than after.
//!
//! ## What enforcement means
//!
//! Enforcement is off by default and behind a flag for milestone 1, because
//! recording history is worth having long before anyone's work is refused over
//! it. When it is on, only **billable** tools on a **hard-stop** plan are
//! refused, and only past the bucket. Reads keep working in every case, so an
//! org that hits its limit mid-task can still see the state of its queue,
//! finish reasoning about it, and go and upgrade — rather than being locked out
//! of its own data by a counter.

use df_core::ids::UserId;
use df_core::usage::{PeriodUsage, PlanLimits};
use df_core::Tx;
use serde::Serialize;

use crate::classify::{self, Class};
use crate::error::{BillingError, Result};

/// Fraction of the bucket at which a caller starts being warned.
///
/// Eighty percent is early enough that a team has time to do something about it
/// before work starts being refused, and late enough that it is not noise.
pub const WARN_AT: f64 = 0.8;

/// Configuration for the meter.
#[derive(Debug, Clone)]
pub struct Meter {
    /// Refuse billable calls past a hard-stop bucket. Off by default.
    pub enforce: bool,
    /// Where a caller who has run out is told to go. Named in the refusal,
    /// because an error that says "upgrade" without saying where is a dead end
    /// for an agent and an annoyance for a human.
    pub upgrade_url: String,
}

impl Meter {
    pub fn new(enforce: bool, upgrade_url: impl Into<String>) -> Self {
        Self {
            enforce,
            upgrade_url: upgrade_url.into(),
        }
    }

    /// Charge one tool call, or refuse it.
    ///
    /// Returns what the caller needs to report: whether it was billable, and
    /// where the org now stands against its bucket.
    pub async fn charge(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<Charge> {
        let class = classify::classify(tool);
        let limits = tx.plan_limits().await?;
        self.check_quota(tx, tool, class, &limits).await?;

        let usage = tx
            .record_usage(Some(user), tool, class.is_billable())
            .await?;

        Ok(Charge::new(class, usage, limits))
    }

    /// Check whether a call would be refused, without recording any usage.
    ///
    /// Used by `sync_ticket` (`df_mcp::tools::jobs`), whose "work" is an
    /// outbound tracker write that has already happened by the time `charge`
    /// runs (see `Factory::charge`'s doc comment) — without this, an org on a
    /// hard-stop plan already over its bucket gets that write posted anyway,
    /// only to be told afterwards that it wasn't billed. This reads the same
    /// counters `charge` checks before recording usage, so the two can
    /// disagree only if the count changes in between — a narrow, acceptable
    /// race no different from any other check-then-act gap, and strictly
    /// better than not checking at all.
    pub async fn would_refuse(&self, tx: &mut Tx<'_>, tool: &str) -> Result<()> {
        let class = classify::classify(tool);
        let limits = tx.plan_limits().await?;
        self.check_quota(tx, tool, class, &limits).await
    }

    /// The enforcement/hard-stop/bucket check shared by [`Self::charge`] and
    /// [`Self::would_refuse`].
    ///
    /// The check reads the count *before* this call is added, so the call
    /// that lands exactly on the limit is allowed and the next one is not. An
    /// off-by-one here is the difference between a plan advertised as 500
    /// operations delivering 500 or 499.
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

    /// Report an org's standing without charging for anything.
    ///
    /// Used by the `usage` and `whoami` tools, which are themselves free — a
    /// caller must never have to spend an operation to find out how many it has
    /// left.
    pub async fn report(&self, tx: &mut Tx<'_>) -> Result<Status> {
        let usage = tx.current_usage().await?;
        let limits = tx.plan_limits().await?;
        Ok(Status::new(usage, limits, self.enforce))
    }
}

/// Where an org stands against its bucket.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub plan: String,
    /// Billable operations included this month, after any negotiated override.
    pub included_ops: i64,
    pub billable_used: i64,
    /// Never negative: an org past its bucket has none left, not a debt.
    pub remaining: i64,
    /// Every recorded call this month, billable or not.
    pub total_calls: i64,
    pub period_start: chrono::NaiveDate,
    /// True past [`WARN_AT`] of the bucket. Worth surfacing to a human.
    pub warning: bool,
    /// Whether exceeding the bucket stops billable work rather than metering
    /// overage.
    pub hard_stop: bool,
    /// Whether the server is currently refusing calls over the bucket at all.
    /// False during milestone 1 unless the operator turns it on.
    pub enforced: bool,
}

impl Status {
    fn new(usage: PeriodUsage, limits: PlanLimits, enforced: bool) -> Self {
        Self {
            plan: limits.display_name,
            included_ops: limits.included_ops,
            billable_used: usage.billable_count,
            remaining: remaining(usage.billable_count, limits.included_ops),
            total_calls: usage.total_count,
            period_start: usage.period_start,
            warning: warning(usage.billable_count, limits.included_ops),
            hard_stop: limits.hard_stop,
            enforced,
        }
    }
}

/// The outcome of charging one call.
#[derive(Debug, Clone, PartialEq)]
pub struct Charge {
    pub billable: bool,
    pub status: Status,
}

impl Charge {
    fn new(class: Class, usage: PeriodUsage, limits: PlanLimits) -> Self {
        Self {
            billable: class.is_billable(),
            // `enforced` is not carried here: a `Charge` describes a call that
            // was already allowed, and the flag only decides whether one is
            // refused. `Status::enforced` is meaningful in a report.
            status: Status::new(usage, limits, false),
        }
    }
}

/// Operations left in the bucket, floored at zero.
fn remaining(used: i64, included: i64) -> i64 {
    included.saturating_sub(used).max(0)
}

/// Whether an org has crossed the warning threshold.
///
/// A bucket of zero counts as exhausted rather than as a division by zero — an
/// org with no included operations is over its limit from the first call.
fn warning(used: i64, included: i64) -> bool {
    if included <= 0 {
        return true;
    }
    used as f64 >= included as f64 * WARN_AT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_never_goes_negative() {
        assert_eq!(remaining(0, 500), 500);
        assert_eq!(remaining(499, 500), 1);
        assert_eq!(remaining(500, 500), 0);
        assert_eq!(
            remaining(900, 500),
            0,
            "an org past its bucket has none left, not a debt"
        );
    }

    /// The boundary customers actually notice: a plan sold as 500 operations
    /// has to deliver 500, not 499.
    #[test]
    fn the_warning_starts_at_four_fifths() {
        assert!(!warning(399, 500));
        assert!(warning(400, 500));
        assert!(warning(500, 500));
    }

    #[test]
    fn an_empty_bucket_is_always_over() {
        assert!(warning(0, 0));
        assert_eq!(remaining(0, 0), 0);
    }
}
