//! Jobs — the queue.
//!
//! Every job is anchored to a repo. The lifecycle is
//! `pending → in-progress → completed | failed`, with `repend` returning a
//! terminal job to `pending` for one more dispatch.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::ids::{JobId, OrgId, RepoId, TeamId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "job_status", rename_all = "lowercase")]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pending,
    #[sqlx(rename = "in-progress")]
    #[serde(rename = "in-progress")]
    InProgress,
    Completed,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::InProgress => "in-progress",
            Status::Completed => "completed",
            Status::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Completed | Status::Failed)
    }
}

impl std::str::FromStr for Status {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Status::Pending),
            "in-progress" => Ok(Status::InProgress),
            "completed" => Ok(Status::Completed),
            "failed" => Ok(Status::Failed),
            other => Err(Error::Invalid(format!(
                "unknown status {other:?} (expected pending | in-progress | completed | failed)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "tracker", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Tracker {
    Jira,
    Github,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: JobId,
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub team_id: Option<TeamId>,
    pub title: String,
    pub description: Option<String>,
    pub status: Status,
    pub ticket_ref: Option<String>,
    pub tracker: Option<Tracker>,
    pub agent_type: Option<String>,
    /// Opaque to dark-factory. Customers' own skills own the shape.
    pub metadata: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub attempts: i32,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub claimed_by: Option<UserId>,
    pub claimed_by_label: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub repo_id: RepoId,
    pub team_id: Option<TeamId>,
    pub title: String,
    pub description: Option<String>,
    pub ticket_ref: Option<String>,
    pub tracker: Option<Tracker>,
    pub agent_type: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Job ids that must reach `completed` before this one is claimable.
    pub depends_on: Vec<JobId>,
    pub created_by: Option<UserId>,
}

#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    pub status: Option<Status>,
    pub repo_id: Option<RepoId>,
    pub team_id: Option<TeamId>,
    /// Restrict to jobs this user created. Used by "what did I queue?" views.
    pub created_by: Option<UserId>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub pending: i64,
    pub in_progress: i64,
    pub completed: i64,
    pub failed: i64,
    pub blocked: i64,
    pub total: i64,
}

const JOB_COLS: &str = "id, org_id, repo_id, team_id, title, description, status, ticket_ref, \
                        tracker, agent_type, metadata, created_at, started_at, completed_at, \
                        attempts, result, error, created_by, claimed_by, claimed_by_label";

impl Tx<'_> {
    /// Enqueue a job.
    ///
    /// The id comes from the org's own counter, bumped under that org's row lock
    /// inside this transaction — so ids are dense and per-tenant, and two
    /// concurrent inserts in the same org cannot collide. Inserts in *different*
    /// orgs take different row locks and do not contend.
    pub async fn add_job(&mut self, new: NewJob) -> Result<Job> {
        if new.title.trim().is_empty() {
            return Err(Error::Invalid("job title must not be empty".into()));
        }

        let org = self.org();

        // Confirm the repo belongs to this org before anything else. RLS would
        // also catch it, but a foreign-key error is a far worse message for an
        // agent than "repo not found".
        let repo = self
            .get_repo(new.repo_id)
            .await?
            .ok_or_else(|| Error::RepoNotFound(new.repo_id.to_string()))?;

        let seq: i64 = sqlx::query_scalar(
            "UPDATE orgs SET next_job_seq = next_job_seq + 1 WHERE id = $1 \
             RETURNING next_job_seq - 1",
        )
        .bind(org)
        .fetch_optional(self.conn())
        .await?
        .ok_or(Error::OrgNotFound(org))?;

        let id = JobId::from_seq(seq);
        // Inherit the repo's team unless the caller named one, so team-scoped
        // reads work without the caller having to know about teams at all.
        let team_id = new.team_id.or(repo.team_id);

        let job: Job = sqlx::query_as(&format!(
            "INSERT INTO jobs (id, org_id, repo_id, team_id, title, description, ticket_ref, \
                               tracker, agent_type, metadata, created_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING {JOB_COLS}"
        ))
        .bind(&id)
        .bind(org)
        .bind(new.repo_id)
        .bind(team_id)
        .bind(new.title.trim())
        .bind(new.description.as_deref())
        .bind(new.ticket_ref.as_deref())
        .bind(new.tracker)
        .bind(new.agent_type.as_deref())
        .bind(new.metadata.unwrap_or_else(|| serde_json::json!({})))
        .bind(new.created_by)
        .fetch_one(self.conn())
        .await?;

        if !new.depends_on.is_empty() {
            self.set_dependencies(&id, &new.depends_on, &[]).await?;
        }

        Ok(job)
    }

    pub async fn get_job(&mut self, id: &JobId) -> Result<Job> {
        let org = self.org();
        sqlx::query_as(&format!(
            "SELECT {JOB_COLS} FROM jobs WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::JobNotFound(id.clone()))
    }

    /// Look a job up by its tracker ticket reference. Returns the most recent
    /// match: a ticket may legitimately be queued more than once (a retry, a
    /// follow-up), and the newest is the one an agent means.
    pub async fn get_job_by_ticket(&mut self, ticket_ref: &str) -> Result<Option<Job>> {
        let org = self.org();
        let job = sqlx::query_as(&format!(
            "SELECT {JOB_COLS} FROM jobs WHERE org_id = $1 AND ticket_ref = $2 \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(org)
        .bind(ticket_ref)
        .fetch_optional(self.conn())
        .await?;
        Ok(job)
    }

    pub async fn list_jobs(&mut self, f: &JobFilter) -> Result<Vec<Job>> {
        let org = self.org();
        let jobs = sqlx::query_as(&format!(
            "SELECT {JOB_COLS} FROM jobs \
             WHERE org_id = $1 \
               AND ($2::job_status IS NULL OR status = $2) \
               AND ($3::uuid IS NULL OR repo_id = $3) \
               AND ($4::uuid IS NULL OR team_id = $4) \
               AND ($5::uuid IS NULL OR created_by = $5) \
             ORDER BY created_at DESC \
             LIMIT $6"
        ))
        .bind(org)
        .bind(f.status)
        .bind(f.repo_id)
        .bind(f.team_id)
        .bind(f.created_by)
        .bind(f.limit.unwrap_or(200).clamp(1, 1000))
        .fetch_all(self.conn())
        .await?;
        Ok(jobs)
    }

    /// Claim one or more pending jobs atomically.
    ///
    /// All or nothing: if any requested job is missing, already claimed, or
    /// still blocked, the whole batch fails and none are claimed. A partial
    /// claim would leave an agent believing it owns work it does not, which is
    /// the exact race this queue exists to prevent.
    ///
    /// Rows are locked in a deterministic (sorted) order so two agents claiming
    /// overlapping batches cannot deadlock each other.
    pub async fn claim_jobs(
        &mut self,
        ids: &[JobId],
        claimer: UserId,
        label: Option<&str>,
    ) -> Result<Vec<Job>> {
        if ids.is_empty() {
            return Err(Error::Invalid(
                "claim_jobs needs at least one job id".into(),
            ));
        }

        let org = self.org();
        let mut sorted: Vec<String> = ids.iter().map(|i| i.0.clone()).collect();
        sorted.sort();
        sorted.dedup();

        let locked: Vec<(String, Status)> = sqlx::query_as(
            "SELECT id, status FROM jobs WHERE org_id = $1 AND id = ANY($2) \
             ORDER BY id FOR UPDATE",
        )
        .bind(org)
        .bind(&sorted)
        .fetch_all(self.conn())
        .await?;

        if locked.len() != sorted.len() {
            let found: std::collections::HashSet<&str> =
                locked.iter().map(|(i, _)| i.as_str()).collect();
            let missing = sorted
                .iter()
                .find(|i| !found.contains(i.as_str()))
                .cloned()
                .unwrap_or_default();
            return Err(Error::JobNotFound(JobId(missing)));
        }

        for (id, status) in &locked {
            if *status != Status::Pending {
                return Err(Error::WrongStatus {
                    job: JobId(id.clone()),
                    actual: status.as_str().to_string(),
                    expected: "pending".into(),
                });
            }
        }

        // Reject anything still blocked by an incomplete dependency.
        let blocked: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT d.job_id FROM job_dependencies d \
             JOIN jobs dep ON dep.org_id = d.org_id AND dep.id = d.depends_on \
             WHERE d.org_id = $1 AND d.job_id = ANY($2) AND dep.status <> 'completed'",
        )
        .bind(org)
        .bind(&sorted)
        .fetch_all(self.conn())
        .await?;

        if let Some(b) = blocked.first() {
            return Err(Error::WrongStatus {
                job: JobId(b.clone()),
                actual: "blocked by an incomplete dependency".into(),
                expected: "ready".into(),
            });
        }

        let jobs: Vec<Job> = sqlx::query_as(&format!(
            "UPDATE jobs SET status = 'in-progress', started_at = now(), \
                    attempts = attempts + 1, claimed_by = $3, claimed_by_label = $4 \
             WHERE org_id = $1 AND id = ANY($2) RETURNING {JOB_COLS}"
        ))
        .bind(org)
        .bind(&sorted)
        .bind(claimer)
        .bind(label)
        .fetch_all(self.conn())
        .await?;

        Ok(jobs)
    }

    /// Mark an in-progress job completed.
    pub async fn complete_job(&mut self, id: &JobId, result: Option<&str>) -> Result<Job> {
        self.finalize(id, Status::Completed, result, None).await
    }

    /// Mark an in-progress job failed.
    pub async fn fail_job(&mut self, id: &JobId, error: Option<&str>) -> Result<Job> {
        self.finalize(id, Status::Failed, None, error).await
    }

    async fn finalize(
        &mut self,
        id: &JobId,
        to: Status,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<Job> {
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
            "UPDATE jobs SET status = $3, completed_at = now(), result = $4, error = $5 \
             WHERE org_id = $1 AND id = $2 RETURNING {JOB_COLS}"
        ))
        .bind(org)
        .bind(id)
        .bind(to)
        .bind(result)
        .bind(error)
        .fetch_one(self.conn())
        .await?;

        Ok(job)
    }

    /// Return a terminal job to `pending` for one more dispatch. `attempts` is
    /// preserved so a job that keeps coming back is visible as such.
    pub async fn repend_job(&mut self, id: &JobId) -> Result<Job> {
        let org = self.org();
        let current: Option<Status> =
            sqlx::query_scalar("SELECT status FROM jobs WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org)
                .bind(id)
                .fetch_optional(self.conn())
                .await?;

        let current = current.ok_or_else(|| Error::JobNotFound(id.clone()))?;
        if current == Status::Pending {
            return Err(Error::WrongStatus {
                job: id.clone(),
                actual: "pending".into(),
                expected: "completed, failed, or in-progress".into(),
            });
        }

        let job = sqlx::query_as(&format!(
            "UPDATE jobs SET status = 'pending', started_at = NULL, completed_at = NULL, \
                    result = NULL, error = NULL, claimed_by = NULL, claimed_by_label = NULL \
             WHERE org_id = $1 AND id = $2 RETURNING {JOB_COLS}"
        ))
        .bind(org)
        .bind(id)
        .fetch_one(self.conn())
        .await?;

        Ok(job)
    }

    pub async fn delete_job(&mut self, id: &JobId) -> Result<()> {
        let org = self.org();
        let n = sqlx::query("DELETE FROM jobs WHERE org_id = $1 AND id = $2")
            .bind(org)
            .bind(id)
            .execute(self.conn())
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::JobNotFound(id.clone()));
        }
        Ok(())
    }

    /// Edit a pending job. Every field is optional; `None` leaves it unchanged.
    pub async fn update_job(
        &mut self,
        id: &JobId,
        title: Option<&str>,
        description: Option<&str>,
        agent_type: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<Job> {
        if title.is_none() && description.is_none() && agent_type.is_none() && metadata.is_none() {
            return Err(Error::Invalid(
                "update_job needs at least one of title, description, agentType, metadata".into(),
            ));
        }

        let org = self.org();
        let job = sqlx::query_as(&format!(
            "UPDATE jobs SET title = COALESCE($3, title), \
                    description = COALESCE($4, description), \
                    agent_type = COALESCE($5, agent_type), \
                    metadata = COALESCE($6, metadata) \
             WHERE org_id = $1 AND id = $2 RETURNING {JOB_COLS}"
        ))
        .bind(org)
        .bind(id)
        .bind(title)
        .bind(description)
        .bind(agent_type)
        .bind(metadata)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::JobNotFound(id.clone()))?;

        Ok(job)
    }

    /// Add and/or remove dependencies, rejecting anything that would create a
    /// cycle.
    ///
    /// The check runs *after* the inserts inside this transaction and rolls back
    /// on failure. Checking reachability before inserting would race a
    /// concurrent edit that closes the loop from the other side; letting the
    /// database hold the rows and then asking "is this job now reachable from
    /// itself?" cannot.
    pub async fn set_dependencies(
        &mut self,
        id: &JobId,
        add: &[JobId],
        remove: &[JobId],
    ) -> Result<Vec<JobId>> {
        if add.is_empty() && remove.is_empty() {
            return Err(Error::Invalid(
                "set_dependencies needs at least one dependency to add or remove".into(),
            ));
        }

        let org = self.org();

        for dep in add {
            if dep == id {
                return Err(Error::DependencyCycle(id.clone(), id.clone()));
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM jobs WHERE org_id = $1 AND id = $2)",
            )
            .bind(org)
            .bind(dep)
            .fetch_one(self.conn())
            .await?;
            if !exists {
                return Err(Error::JobNotFound(dep.clone()));
            }

            sqlx::query(
                "INSERT INTO job_dependencies (org_id, job_id, depends_on) VALUES ($1,$2,$3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(org)
            .bind(id)
            .bind(dep)
            .execute(self.conn())
            .await?;
        }

        for dep in remove {
            sqlx::query(
                "DELETE FROM job_dependencies WHERE org_id = $1 AND job_id = $2 AND depends_on = $3",
            )
            .bind(org)
            .bind(id)
            .bind(dep)
            .execute(self.conn())
            .await?;
        }

        // Is `id` now reachable from itself by following depends_on edges?
        let cycle: Option<String> = sqlx::query_scalar(
            "WITH RECURSIVE reach(job_id) AS ( \
                 SELECT depends_on FROM job_dependencies WHERE org_id = $1 AND job_id = $2 \
                 UNION \
                 SELECT d.depends_on FROM job_dependencies d \
                   JOIN reach r ON r.job_id = d.job_id WHERE d.org_id = $1 \
             ) SELECT job_id FROM reach WHERE job_id = $2 LIMIT 1",
        )
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;

        if let Some(via) = cycle {
            return Err(Error::DependencyCycle(id.clone(), JobId(via)));
        }

        let deps: Vec<String> = sqlx::query_scalar(
            "SELECT depends_on FROM job_dependencies WHERE org_id = $1 AND job_id = $2 \
             ORDER BY depends_on",
        )
        .bind(org)
        .bind(id)
        .fetch_all(self.conn())
        .await?;

        Ok(deps.into_iter().map(JobId).collect())
    }

    pub async fn dependencies_of(&mut self, id: &JobId) -> Result<Vec<JobId>> {
        let org = self.org();
        let deps: Vec<String> = sqlx::query_scalar(
            "SELECT depends_on FROM job_dependencies WHERE org_id = $1 AND job_id = $2 \
             ORDER BY depends_on",
        )
        .bind(org)
        .bind(id)
        .fetch_all(self.conn())
        .await?;
        Ok(deps.into_iter().map(JobId).collect())
    }

    /// Pending jobs whose dependencies are all completed — i.e. claimable now.
    pub async fn ready(&mut self, repo_id: Option<RepoId>) -> Result<Vec<Job>> {
        let org = self.org();
        let jobs = sqlx::query_as(&format!(
            "SELECT {JOB_COLS} FROM jobs j \
             WHERE j.org_id = $1 AND j.status = 'pending' \
               AND ($2::uuid IS NULL OR j.repo_id = $2) \
               AND NOT EXISTS ( \
                 SELECT 1 FROM job_dependencies d \
                 JOIN jobs dep ON dep.org_id = d.org_id AND dep.id = d.depends_on \
                 WHERE d.org_id = j.org_id AND d.job_id = j.id AND dep.status <> 'completed') \
             ORDER BY j.created_at"
        ))
        .bind(org)
        .bind(repo_id)
        .fetch_all(self.conn())
        .await?;
        Ok(jobs)
    }

    /// Pending jobs still waiting on at least one incomplete dependency.
    pub async fn blocked(&mut self, repo_id: Option<RepoId>) -> Result<Vec<Job>> {
        let org = self.org();
        let jobs = sqlx::query_as(&format!(
            "SELECT {JOB_COLS} FROM jobs j \
             WHERE j.org_id = $1 AND j.status = 'pending' \
               AND ($2::uuid IS NULL OR j.repo_id = $2) \
               AND EXISTS ( \
                 SELECT 1 FROM job_dependencies d \
                 JOIN jobs dep ON dep.org_id = d.org_id AND dep.id = d.depends_on \
                 WHERE d.org_id = j.org_id AND d.job_id = j.id AND dep.status <> 'completed') \
             ORDER BY j.created_at"
        ))
        .bind(org)
        .bind(repo_id)
        .fetch_all(self.conn())
        .await?;
        Ok(jobs)
    }

    pub async fn stats(&mut self, repo_id: Option<RepoId>) -> Result<Stats> {
        let org = self.org();
        let stats = sqlx::query_as(
            "SELECT \
               COUNT(*) FILTER (WHERE status = 'pending')     AS pending, \
               COUNT(*) FILTER (WHERE status = 'in-progress') AS in_progress, \
               COUNT(*) FILTER (WHERE status = 'completed')   AS completed, \
               COUNT(*) FILTER (WHERE status = 'failed')      AS failed, \
               COUNT(*) FILTER (WHERE status = 'pending' AND EXISTS ( \
                 SELECT 1 FROM job_dependencies d \
                 JOIN jobs dep ON dep.org_id = d.org_id AND dep.id = d.depends_on \
                 WHERE d.org_id = j.org_id AND d.job_id = j.id \
                   AND dep.status <> 'completed'))            AS blocked, \
               COUNT(*)                                       AS total \
             FROM jobs j WHERE j.org_id = $1 AND ($2::uuid IS NULL OR j.repo_id = $2)",
        )
        .bind(org)
        .bind(repo_id)
        .fetch_one(self.conn())
        .await?;
        Ok(stats)
    }
}
