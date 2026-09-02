//! Job tools — the queue itself.
//!
//! The lifecycle is `pending → in-progress → completed | failed`, with
//! `repend_job` returning a terminal job to `pending` for one more dispatch.
//! The one rule an agent has to internalize is that **claiming is what makes
//! work yours**: `ready` shows what is claimable, `claim_jobs` takes it
//! atomically, and starting work you have not claimed is how two agents end up
//! writing the same file.

use df_core::ids::JobId;
use df_core::jobs::{JobFilter, NewJob, Status};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{maybe_repo_of, out, repo_of, scope};
use crate::server::{Factory, McpResult};

/// Turn caller-supplied job ids into the domain type.
fn ids(raw: Vec<String>) -> Vec<JobId> {
    raw.into_iter().map(JobId::from).collect()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddJobArgs {
    /// One line saying what needs doing. This is what other agents and humans
    /// see in listings, so make it specific.
    pub title: String,
    /// Everything the agent that picks this up will need. It may have none of
    /// your context.
    #[serde(default)]
    pub description: Option<String>,
    /// Registered repo slug this job belongs to.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or the git remote URL identifying it, from `git remote get-url origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Issue or ticket this job corresponds to, as a free-form reference such
    /// as "PROJ-123" or "acme/api#42". Recorded, not resolved.
    #[serde(default)]
    pub ticket_ref: Option<String>,
    /// Free-form hint about which kind of agent should take this. Never
    /// enforced — any agent may claim any job.
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Arbitrary JSON this server stores and never interprets. Your own
    /// workflow's fields go here.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Job ids that must be completed before this one can be claimed.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobArgs {
    /// The job id, like "job-42".
    pub job: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsArgs {
    /// Only jobs in this state: "pending", "in-progress", "completed" or
    /// "failed". Omit for all of them.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// Only jobs you queued yourself. Defaults to false.
    #[serde(default)]
    pub mine: bool,
    /// Maximum rows. Defaults to the server's own limit.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeArgs {
    /// Narrow to one repo by slug. Omit to cover the whole organization.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or narrow by git remote URL.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJobArgs {
    pub job: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Replaces the whole metadata object; it is not merged.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimJobsArgs {
    /// The job ids to take. All or none succeed.
    pub jobs: Vec<String>,
    /// How you want to be identified to teammates looking at the queue, for
    /// example "api-agent@ci-7". Free-form.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompleteJobArgs {
    pub job: String,
    /// What you did, for whoever reads this later.
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailJobArgs {
    pub job: String,
    /// Why it failed, specifically enough that the next attempt can do better.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetDependenciesArgs {
    /// The job whose dependencies are changing.
    pub job: String,
    /// Job ids this job should wait for.
    #[serde(default)]
    pub add: Vec<String>,
    /// Job ids it should stop waiting for.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[tool_router(router = jobs_router, vis = "pub(crate)")]
impl Factory {
    #[tool(
        name = "add_job",
        description = "Queue a new job against a repository. Give it a specific title and \
                       enough description that an agent with none of your context can pick it \
                       up. Use dependsOn for work that must wait for other jobs. Returns the \
                       created job, including its id."
    )]
    pub async fn add_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AddJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let repo = repo_of(&mut tx, args.repo, args.remote).await?;
        let job = tx
            .add_job(NewJob {
                repo_id: repo.id,
                title: args.title,
                description: args.description,
                ticket_ref: args.ticket_ref,
                agent_type: args.agent_type,
                metadata: args.metadata,
                depends_on: ids(args.depends_on),
                created_by: Some(caller.user_id),
                ..Default::default()
            })
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "get_job",
        description = "Fetch one job by id, with its full description, metadata, status and who \
                       claimed it."
    )]
    pub async fn get_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let job = tx.get_job(&JobId::from(args.job)).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "list_jobs",
        description = "List jobs, optionally filtered by status, repository, or whether you \
                       queued them. To find work you can actually take, prefer `ready` — this \
                       returns jobs regardless of whether their dependencies are satisfied."
    )]
    pub async fn list_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        // Parse before opening a transaction: a bad status is the caller's
        // mistake and does not need a database round trip to detect.
        let status = args
            .status
            .as_deref()
            .map(str::parse::<Status>)
            .transpose()
            .mcp()?;

        let mut tx = self.tx(&caller).await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx
            .list_jobs(&JobFilter {
                status,
                repo_id,
                created_by: args.mine.then_some(caller.user_id),
                limit: args.limit,
                ..Default::default()
            })
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "update_job",
        description = "Change a job's title, description, agent type hint, or metadata. Only \
                       the fields you pass change, except metadata, which is replaced whole. \
                       Does not change status — use claim_jobs, complete_job, fail_job or \
                       repend_job for that."
    )]
    pub async fn update_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let job = tx
            .update_job(
                &JobId::from(args.job),
                args.title.as_deref(),
                args.description.as_deref(),
                args.agent_type.as_deref(),
                args.metadata.as_ref(),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "delete_job",
        description = "Remove a job from the queue permanently. Prefer fail_job for work that \
                       was attempted and did not succeed: deleting loses the history of why."
    )]
    pub async fn delete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::DeletedOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = JobId::from(args.job);
        let mut tx = self.tx(&caller).await?;
        tx.delete_job(&id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DeletedOut { deleted: id }))
    }

    #[tool(
        name = "claim_jobs",
        description = "Atomically take one or more pending jobs, moving them to in-progress \
                       under your name. All of them succeed or none do, so a partial claim can \
                       never leave you believing you own work you do not. Fails if any job is \
                       already claimed or still blocked by an unfinished dependency. Claim \
                       before you start working."
    )]
    pub async fn claim_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClaimJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let jobs = tx
            .claim_jobs(&ids(args.jobs), caller.user_id, args.agent.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "complete_job",
        description = "Mark a job you claimed as completed, with a summary of what was done. \
                       Anything that depends on it becomes claimable."
    )]
    pub async fn complete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CompleteJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let job = tx
            .complete_job(&JobId::from(args.job), args.result.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "fail_job",
        description = "Mark a job you claimed as failed, recording why. Use this rather than \
                       leaving a job in-progress when you cannot finish it — an abandoned claim \
                       blocks everything downstream and tells nobody anything."
    )]
    pub async fn fail_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FailJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let job = tx
            .fail_job(&JobId::from(args.job), args.error.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "repend_job",
        description = "Return a completed or failed job to pending so it can be claimed again. \
                       The attempt count is preserved, so repeated failures stay visible."
    )]
    pub async fn repend_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let job = tx.repend_job(&JobId::from(args.job)).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "set_dependencies",
        description = "Add or remove the jobs one job waits for. A job with unfinished \
                       dependencies cannot be claimed. A change that would make a job depend \
                       on itself, directly or through a chain, is rejected. Returns the \
                       resulting dependency list."
    )]
    pub async fn set_dependencies(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetDependenciesArgs>,
    ) -> Result<Json<out::DependenciesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let job = JobId::from(args.job);
        let mut tx = self.tx(&caller).await?;
        let deps = tx
            .set_dependencies(&job, &ids(args.add), &ids(args.remove))
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DependenciesOut {
            job,
            dependencies: deps,
        }))
    }

    #[tool(
        name = "ready",
        description = "List the jobs that can be claimed right now: pending, with every \
                       dependency completed. This is the tool to call when looking for work."
    )]
    pub async fn ready(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx.ready(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "blocked",
        description = "List pending jobs that cannot be claimed yet because something they \
                       depend on is unfinished. Useful for working out what to unblock first."
    )]
    pub async fn blocked(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx.blocked(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "stats",
        description = "Counts of jobs by state — pending, in-progress, completed, failed, and \
                       how many of the pending ones are blocked — for the whole organization or \
                       one repository."
    )]
    pub async fn stats(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::StatsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let stats = tx.stats(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::StatsOut { stats }))
    }
}
