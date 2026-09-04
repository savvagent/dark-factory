//! The queue, read-only.
//!
//! The console watches the queue; it does not drive it. Every write — enqueue,
//! claim, complete, fail, repend — belongs to the MCP surface, because the
//! agent doing the work is the only party that knows when the work is done.
//! Putting a "mark complete" button here would put a human in a position to lie
//! to the queue about state they cannot observe.
//!
//! So this module is three `GET`s: the list, one job, and the counters behind
//! the overview page. They exist because "why is my agent waiting?" is a
//! question a person asks in a browser, and answering it by telling them to
//! attach an MCP client is not an answer.
//!
//! **Filters name slugs, not ids.** A console URL carries `?repo=api-gateway`,
//! not a UUID, for the same reason repo and team routes take slugs: a slug is
//! what a person types and what a link can be shared as. An unregistered slug
//! is an error listing what *is* registered — `resolve_repo` and `resolve_team`
//! already write that message, and a filter that silently matched nothing would
//! render an empty queue that looks like a quiet one.

use std::str::FromStr;

use axum::extract::{Json, Path, Query, State};
use df_core::ids::JobId;
use df_core::jobs::{Job, JobFilter, Stats, Status};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::session::OrgCtx;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsQuery {
    /// `pending` | `in-progress` | `active` | `completed` | `failed`.
    #[serde(default)]
    pub status: Option<String>,
    /// A registered repo slug.
    #[serde(default)]
    pub repo: Option<String>,
    /// A team slug.
    #[serde(default)]
    pub team: Option<String>,
    /// Only jobs this account queued. The "what did I ask for?" view.
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsQuery {
    #[serde(default)]
    pub repo: Option<String>,
}

/// `GET /api/orgs/{org}/jobs` — the queue, newest first.
pub async fn list_jobs(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Query(q): Query<ListJobsQuery>,
) -> ApiResult<Json<Vec<Job>>> {
    // Parsed before the transaction opens: a typo in a query string should not
    // cost a pooled connection, and `Status::from_str` already names the four
    // valid values, which is what an error here has to do.
    let status = q
        .status
        .as_deref()
        .map(Status::from_str)
        .transpose()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let mut tx = state.db.begin(ctx.org.id).await?;

    let repo_id = match &q.repo {
        Some(slug) => Some(
            tx.resolve_repo(&df_core::repos::RepoRef {
                slug: Some(slug.clone()),
                remote: None,
            })
            .await?
            .id,
        ),
        None => None,
    };

    let team_id = match &q.team {
        Some(slug) => Some(tx.resolve_team(slug).await?.id),
        None => None,
    };

    let jobs = tx
        .list_jobs(&JobFilter {
            status,
            repo_id,
            team_id,
            created_by: q.mine.then_some(ctx.user.id),
            limit: q.limit,
        })
        .await?;
    tx.commit().await?;

    Ok(Json(jobs))
}

/// `GET /api/orgs/{org}/jobs/stats` — the counters behind the overview.
///
/// `blocked` is the one that is not a status: it counts pending jobs still
/// waiting on a dependency, which is the difference between a queue that is
/// idle and a queue that is stuck.
pub async fn job_stats(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Query(q): Query<StatsQuery>,
) -> ApiResult<Json<Stats>> {
    let mut tx = state.db.begin(ctx.org.id).await?;

    let repo_id = match &q.repo {
        Some(slug) => Some(
            tx.resolve_repo(&df_core::repos::RepoRef {
                slug: Some(slug.clone()),
                remote: None,
            })
            .await?
            .id,
        ),
        None => None,
    };

    let stats = tx.stats(repo_id).await?;
    tx.commit().await?;

    Ok(Json(stats))
}

/// `GET /api/orgs/{org}/jobs/{job}` — one job, with the ids it waits on.
pub async fn get_job(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, id)): Path<(String, String)>,
) -> ApiResult<Json<JobDetail>> {
    let id = JobId::from(id);

    let mut tx = state.db.begin(ctx.org.id).await?;
    let job = tx.get_job(&id).await?;
    let depends_on = tx.dependencies_of(&id).await?;
    tx.commit().await?;

    Ok(Json(JobDetail { job, depends_on }))
}

/// One job plus its dependencies.
///
/// Flattened rather than nested, so a caller that only wants the job reads it
/// exactly as it reads a list entry. The dependency ids come from a second
/// query and are not on `Job` itself, which is why they cannot simply be a
/// column.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobDetail {
    #[serde(flatten)]
    pub job: Job,
    /// Job ids that must reach `completed` before this one is claimable.
    pub depends_on: Vec<JobId>,
}
