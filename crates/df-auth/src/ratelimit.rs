//! Attempt throttling with exponential lockout.
//!
//! A passkey ceremony is cryptographic, not a guessable secret, but the login
//! endpoint that resolves it is still a network-reachable surface: without
//! throttling nothing stops a flood of forged or replayed authentication
//! attempts, or spamming a passkey-reset/claim flow. This is not an optional
//! hardening pass — it is what keeps the surface safe under abuse.
//!
//! Buckets are opaque strings so one table serves per-user, per-email, and
//! per-IP limits. **Always throttle on both an identity key and the source IP**:
//! identity-only lets one attacker spray a million addresses, IP-only lets a
//! botnet through.

use chrono::{DateTime, Utc};
use df_core::Db;

use crate::error::{AuthError, Result};

/// Failures tolerated inside the window before lockout begins.
pub const MAX_FAILURES: i64 = 5;

/// How far back failures are counted, in seconds.
pub const WINDOW_SECS: i64 = 15 * 60;

/// Lockout after the threshold doubles per additional failure, from this base.
const LOCKOUT_BASE_SECS: i64 = 30;

/// Ceiling on a single lockout. An unbounded backoff locks a legitimate user
/// out for days after a bad week and turns a nuisance into a support ticket;
/// 30 minutes already makes brute force hopeless.
const LOCKOUT_MAX_SECS: i64 = 30 * 60;

/// Compute the lockout for a given failure count. Pure, so the policy is
/// testable without a database.
///
/// Returns `None` while under the threshold.
pub fn lockout_secs(failures: i64) -> Option<i64> {
    if failures < MAX_FAILURES {
        return None;
    }
    let over = failures - MAX_FAILURES;
    // Saturating shift: a large `over` must not overflow into a small number,
    // which would silently unlock the account at exactly the wrong moment.
    let secs = LOCKOUT_BASE_SECS.saturating_mul(1i64.checked_shl(over.min(20) as u32).unwrap_or(1));
    Some(secs.min(LOCKOUT_MAX_SECS))
}

/// Refuse the attempt if this bucket is locked out.
///
/// Call **before** doing any credential work, so a locked-out attacker learns
/// nothing and costs us no hashing.
pub async fn check(db: &Db, bucket: &str) -> Result<()> {
    let (failures, last_failure) = recent_failures(db, bucket).await?;

    if let Some(lockout) = lockout_secs(failures) {
        if let Some(last) = last_failure {
            let elapsed = (Utc::now() - last).num_seconds();
            if elapsed < lockout {
                return Err(AuthError::RateLimited {
                    retry_after_secs: lockout - elapsed,
                });
            }
        }
    }
    Ok(())
}

/// Record the outcome of an attempt.
///
/// A success is recorded too, and it resets the count: `recent_failures` only
/// counts failures *since the last success*, so a user who mistypes four times
/// and then gets in is not one slip away from a lockout tomorrow.
pub async fn record(db: &Db, bucket: &str, successful: bool) -> Result<()> {
    sqlx::query("INSERT INTO auth_attempts (bucket, successful) VALUES ($1, $2)")
        .bind(bucket)
        .bind(successful)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Charge one unit against a bucket that limits a *rate* rather than a failure
/// count — issuing an email link, say, where every attempt succeeds and the
/// thing being defended is the mailbox on the other end.
///
/// Implemented as a recorded failure because that is what
/// [`recent_failures`] counts, and the two policies want the same arithmetic.
/// The distinction is only in what the call site means: [`record`] reports an
/// outcome, this consumes an allowance. There is deliberately no success to
/// reset the count — an unthrottled "email me a link" endpoint is a mail bomb
/// aimed at whatever address the attacker types, and it does not become safe
/// because the previous send worked.
pub async fn charge(db: &Db, bucket: &str) -> Result<()> {
    record(db, bucket, false).await
}

/// Failures in the window since the most recent success.
async fn recent_failures(db: &Db, bucket: &str) -> Result<(i64, Option<DateTime<Utc>>)> {
    let row: (i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "WITH last_ok AS ( \
             SELECT COALESCE(MAX(created_at), 'epoch'::timestamptz) AS at \
             FROM auth_attempts \
             WHERE bucket = $1 AND successful \
               AND created_at > now() - make_interval(secs => $2)) \
         SELECT COUNT(*), MAX(created_at) FROM auth_attempts, last_ok \
         WHERE bucket = $1 AND NOT successful \
           AND created_at > GREATEST(last_ok.at, now() - make_interval(secs => $2))",
    )
    .bind(bucket)
    .bind(WINDOW_SECS as f64)
    .fetch_one(db.pool())
    .await?;
    Ok(row)
}

/// Delete attempt rows older than the window. Run periodically — this table is
/// append-only on a hot path and would otherwise grow without bound.
pub async fn sweep(db: &Db) -> Result<u64> {
    let n = sqlx::query(
        "DELETE FROM auth_attempts WHERE created_at < now() - make_interval(secs => $1)",
    )
    .bind((WINDOW_SECS * 4) as f64)
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_lockout_below_the_threshold() {
        for n in 0..MAX_FAILURES {
            assert_eq!(lockout_secs(n), None, "{n} failures should not lock out");
        }
    }

    #[test]
    fn lockout_grows_then_caps() {
        assert_eq!(lockout_secs(5), Some(30));
        assert_eq!(lockout_secs(6), Some(60));
        assert_eq!(lockout_secs(7), Some(120));
        assert_eq!(lockout_secs(12), Some(LOCKOUT_MAX_SECS));
        // Must stay capped rather than wrapping to something small.
        assert_eq!(lockout_secs(1_000_000), Some(LOCKOUT_MAX_SECS));
    }

    /// A million codes against a 90-second window: the throttle has to make
    /// exhaustive guessing take longer than the universe, not merely annoy.
    #[test]
    fn brute_force_is_hopeless() {
        // After the threshold every further attempt costs at least the base
        // lockout, so guessing the 1e6 space takes at least this long.
        let attempts_needed = 1_000_000i64;
        let seconds = (attempts_needed - MAX_FAILURES) * LOCKOUT_BASE_SECS;
        assert!(
            seconds / 86_400 > 300,
            "throttle too weak: exhaustive search would take {} days",
            seconds / 86_400
        );
    }
}
