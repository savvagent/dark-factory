//! Repo leases — the primitive that stops two agents colliding in one worktree.
//!
//! A lease is **advisory and time-bounded**. dark-factory cannot see git
//! operations, so it cannot prevent a determined agent from working on a leased
//! branch; what it can do is make the collision visible before it happens and
//! give a well-behaved agent somewhere else to go. That limitation is deliberate
//! and is documented to customers rather than hidden — a lease is a coordination
//! signal, not a mutex.
//!
//! Time-bounded matters just as much: an agent that crashes mid-job must not
//! wedge a repository forever. Leases expire, and any acquire reaps the expired
//! ones first.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::ids::{JobId, OrgId, RepoId, UserId};
use serde::Serialize;
use sqlx::FromRow;

/// Default lease lifetime. Long enough that a working agent renewing on a normal
/// cadence never loses its lease mid-task, short enough that a crashed agent
/// frees the branch while a human is still in the room.
pub const DEFAULT_TTL_SECS: i64 = 900;

/// Upper bound on a client-requested TTL. Without a cap, one buggy agent could
/// take an effectively permanent lease and only an admin could clear it.
pub const MAX_TTL_SECS: i64 = 4 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub id: uuid::Uuid,
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub branch: String,
    pub holder_user_id: UserId,
    pub holder_label: Option<String>,
    pub job_id: Option<String>,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub renewed_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub released_at: Option<chrono::DateTime<chrono::Utc>>,
}

const LEASE_COLS: &str = "id, org_id, repo_id, branch, holder_user_id, holder_label, job_id, \
                          acquired_at, renewed_at, expires_at, released_at";

impl Tx<'_> {
    /// Take a lease on `(repo, branch)`.
    ///
    /// Reaps expired leases for this branch first, then inserts. If a live lease
    /// is already held the error names the holder and the expiry, so the caller
    /// can decide between waiting, picking different work, or messaging the
    /// holder — all three are reasonable, and only the agent knows which.
    ///
    /// Re-acquiring a lease you already hold renews it instead of failing, so an
    /// agent that lost track of its own state converges rather than deadlocking
    /// against itself.
    pub async fn acquire_lease(
        &mut self,
        repo_id: RepoId,
        branch: &str,
        holder: UserId,
        label: Option<&str>,
        job_id: Option<&JobId>,
        ttl_secs: Option<i64>,
    ) -> Result<Lease> {
        let branch = branch.trim();
        if branch.is_empty() {
            return Err(Error::Invalid("lease branch must not be empty".into()));
        }
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(60, MAX_TTL_SECS);
        let org = self.org();

        // Reap expired leases on this branch. Marking them released (rather than
        // deleting) keeps the history readable in the console — "who had this
        // branch when the build broke?" is a question people actually ask.
        sqlx::query(
            "UPDATE repo_leases SET released_at = now() \
             WHERE org_id = $1 AND repo_id = $2 AND branch = $3 \
               AND released_at IS NULL AND expires_at <= now()",
        )
        .bind(org)
        .bind(repo_id)
        .bind(branch)
        .execute(self.conn())
        .await?;

        let existing: Option<Lease> = sqlx::query_as(&format!(
            "SELECT {LEASE_COLS} FROM repo_leases \
             WHERE org_id = $1 AND repo_id = $2 AND branch = $3 AND released_at IS NULL \
             FOR UPDATE"
        ))
        .bind(org)
        .bind(repo_id)
        .bind(branch)
        .fetch_optional(self.conn())
        .await?;

        if let Some(live) = existing {
            if live.holder_user_id == holder {
                return self.renew_lease(live.id, holder, Some(ttl)).await;
            }
            return Err(Error::LeaseHeld {
                branch: branch.to_string(),
                holder: live
                    .holder_label
                    .clone()
                    .unwrap_or_else(|| live.holder_user_id.to_string()),
                expires_at: live.expires_at,
            });
        }

        let lease = sqlx::query_as(&format!(
            "INSERT INTO repo_leases (org_id, repo_id, branch, holder_user_id, holder_label, \
                                      job_id, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6, now() + make_interval(secs => $7)) \
             RETURNING {LEASE_COLS}"
        ))
        .bind(org)
        .bind(repo_id)
        .bind(branch)
        .bind(holder)
        .bind(label)
        .bind(job_id.map(|j| j.0.as_str()))
        .bind(ttl as f64)
        .fetch_one(self.conn())
        .await?;

        Ok(lease)
    }

    /// Extend a lease you hold. Only the holder may renew — otherwise any agent
    /// could keep another's lease alive indefinitely.
    pub async fn renew_lease(
        &mut self,
        lease_id: uuid::Uuid,
        holder: UserId,
        ttl_secs: Option<i64>,
    ) -> Result<Lease> {
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(60, MAX_TTL_SECS);
        let org = self.org();

        sqlx::query_as(&format!(
            "UPDATE repo_leases \
             SET renewed_at = now(), expires_at = now() + make_interval(secs => $4) \
             WHERE org_id = $1 AND id = $2 AND holder_user_id = $3 AND released_at IS NULL \
             RETURNING {LEASE_COLS}"
        ))
        .bind(org)
        .bind(lease_id)
        .bind(holder)
        .bind(ttl as f64)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::LeaseNotHeld(lease_id.to_string()))
    }

    pub async fn release_lease(&mut self, lease_id: uuid::Uuid, holder: UserId) -> Result<()> {
        let org = self.org();
        let n = sqlx::query(
            "UPDATE repo_leases SET released_at = now() \
             WHERE org_id = $1 AND id = $2 AND holder_user_id = $3 AND released_at IS NULL",
        )
        .bind(org)
        .bind(lease_id)
        .bind(holder)
        .execute(self.conn())
        .await?
        .rows_affected();

        if n == 0 {
            return Err(Error::LeaseNotHeld(lease_id.to_string()));
        }
        Ok(())
    }

    /// Live leases — "who is in this repo right now?".
    ///
    /// Filters expired rows in the query rather than relying on the reaper, so
    /// the answer is correct even when nothing has tried to acquire recently.
    pub async fn list_leases(&mut self, repo_id: Option<RepoId>) -> Result<Vec<Lease>> {
        let org = self.org();
        let leases = sqlx::query_as(&format!(
            "SELECT {LEASE_COLS} FROM repo_leases \
             WHERE org_id = $1 AND released_at IS NULL AND expires_at > now() \
               AND ($2::uuid IS NULL OR repo_id = $2) \
             ORDER BY acquired_at"
        ))
        .bind(org)
        .bind(repo_id)
        .fetch_all(self.conn())
        .await?;
        Ok(leases)
    }
}
