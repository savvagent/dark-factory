//! Repos — the coordination anchor, from the console side.
//!
//! The same rows the MCP tools `register_repo` and `update_repo` write. There is
//! deliberately no second code path: both surfaces call the same `df-core`
//! functions, so a repo registered by an agent and one registered by a human in
//! the console are the same thing, resolvable by the same remotes.
//!
//! The request types here are df-web's own rather than `df_core::NewRepo`
//! deserialized directly. `NewRepo` carries `created_by`, which the *server*
//! decides from the session — a client that could set it would be able to
//! attribute its registrations to somebody else.

use axum::extract::{Json, Path, State};
use axum::response::{IntoResponse, Response};
use df_core::audit::{action, Entry};
use df_core::ids::TeamId;
use df_core::leases::Lease;
use df_core::repos::{NewRepo, Provider, Repo, RepoPatch};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::session::OrgCtx;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRepoRequest {
    /// The short handle agents will use. Unique per org.
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Every git remote that identifies this repo, in any form git prints.
    /// Normalized before storage, so SSH and HTTPS forms of one repo collapse
    /// to a single row.
    #[serde(default)]
    pub remotes: Vec<String>,
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub team_id: Option<TeamId>,
    #[serde(default)]
    pub default_agent_type: Option<String>,
    #[serde(default)]
    pub tracker_binding: Option<serde_json::Value>,
}

/// A partial update. Absent fields are left alone — see `df_core::RepoPatch`
/// for why this is a PATCH and not a PUT, and why the slug is not in it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRepoRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Absent leaves the team alone; an explicit `null` makes the repo
    /// org-wide. The two have to be distinguishable, or a team-scoped repo can
    /// never be unscoped — and a team with repos still on it cannot be deleted.
    #[serde(default, deserialize_with = "double_option")]
    pub team_id: Option<Option<TeamId>>,
    #[serde(default)]
    pub default_agent_type: Option<String>,
    #[serde(default)]
    pub tracker_binding: Option<serde_json::Value>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub add_remotes: Vec<String>,
}

/// Distinguish "field absent" from "field present and null".
///
/// serde collapses both into `None` for an `Option<T>`; wrapping the whole
/// deserialization in `Some` recovers the difference — absent stays `None`
/// because of `#[serde(default)]`, while an explicit `null` arrives as
/// `Some(None)`.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListReposQuery {
    /// Include repos that have been soft-disabled. Off by default: a retired
    /// repo keeps its job history but should not clutter a picker.
    #[serde(default)]
    pub include_inactive: bool,
}

/// Filter repos down to what the caller is allowed to see.
///
/// Per `docs/specs/2026-09-01-dark-factory-design.md`: a repo with a
/// `team_id` is visible only to that team's members and org admins; a null
/// `team_id` is org-wide. `OrgCtx` only proves org membership, so without this
/// every member — not just the assigned team — could read every team-scoped
/// repo's leases and metadata through the console.
async fn visible_repos(
    tx: &mut df_core::Tx<'_>,
    ctx: &OrgCtx,
    repos: Vec<Repo>,
) -> ApiResult<Vec<Repo>> {
    if ctx.role.can_administer() {
        return Ok(repos);
    }
    let my_teams: std::collections::HashSet<TeamId> = tx
        .list_user_teams(ctx.user.id)
        .await?
        .into_iter()
        .map(|t| t.id)
        .collect();
    Ok(repos
        .into_iter()
        .filter(|r| r.team_id.is_none_or(|t| my_teams.contains(&t)))
        .collect())
}

/// As [`visible_repos`], for a single already-resolved repo. A repo the
/// caller may not see is reported as not found, not forbidden — the same
/// "an org you are not in is 404" rule this file already applies to orgs
/// extends to a team-scoped repo a non-member should not learn exists.
async fn require_visible(tx: &mut df_core::Tx<'_>, ctx: &OrgCtx, repo: Repo) -> ApiResult<Repo> {
    if ctx.role.can_administer() {
        return Ok(repo);
    }
    match repo.team_id {
        None => Ok(repo),
        Some(team) => {
            let is_member = tx
                .list_user_teams(ctx.user.id)
                .await?
                .iter()
                .any(|t| t.id == team);
            if is_member {
                Ok(repo)
            } else {
                Err(ApiError::not_found("no repo with that slug in this org"))
            }
        }
    }
}

/// `GET /api/orgs/{org}/repos`
pub async fn list_repos(
    State(state): State<AppState>,
    ctx: OrgCtx,
    axum::extract::Query(q): axum::extract::Query<ListReposQuery>,
) -> ApiResult<Json<Vec<Repo>>> {
    let mut tx = state.db.begin(ctx.org.id).await?;
    let repos = tx.list_repos(q.include_inactive).await?;
    let repos = visible_repos(&mut tx, &ctx, repos).await?;
    tx.commit().await?;
    Ok(Json(repos))
}

/// `POST /api/orgs/{org}/repos` — register a repo.
pub async fn register_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Json(req): Json<RegisterRepoRequest>,
) -> ApiResult<Response> {
    ctx.require_admin()?;

    let mut tx = state.db.begin(ctx.org.id).await?;
    let repo = tx
        .register_repo(NewRepo {
            slug: req.slug,
            name: req.name,
            remotes: req.remotes,
            provider: req.provider,
            default_branch: req.default_branch,
            team_id: req.team_id,
            default_agent_type: req.default_agent_type,
            tracker_binding: req.tracker_binding,
            // From the session, never from the request.
            created_by: Some(ctx.user.id),
        })
        .await?;

    tx.audit(
        Entry::new(action::REPO_REGISTERED)
            .actor(ctx.user.id)
            .target("repo", repo.slug.clone()),
    )
    .await?;
    tx.commit().await?;

    Ok((http::StatusCode::CREATED, Json(repo)).into_response())
}

/// `GET /api/orgs/{org}/repos/{repo}` — by slug.
pub async fn get_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
) -> ApiResult<Json<Repo>> {
    let mut tx = state.db.begin(ctx.org.id).await?;
    let repo = tx
        .resolve_repo(&df_core::repos::RepoRef {
            slug: Some(slug),
            remote: None,
        })
        .await?;
    let repo = require_visible(&mut tx, &ctx, repo).await?;
    tx.commit().await?;
    Ok(Json(repo))
}

/// `PATCH /api/orgs/{org}/repos/{repo}`
pub async fn update_repo(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
    Json(req): Json<UpdateRepoRequest>,
) -> ApiResult<Json<Repo>> {
    ctx.require_admin()?;

    let mut tx = state.db.begin(ctx.org.id).await?;
    let repo = tx
        .resolve_repo(&df_core::repos::RepoRef {
            slug: Some(slug),
            remote: None,
        })
        .await?;

    let repo = tx
        .update_repo(
            repo.id,
            RepoPatch {
                name: req.name,
                default_branch: req.default_branch,
                team_id: req.team_id,
                default_agent_type: req.default_agent_type,
                tracker_binding: req.tracker_binding,
                active: req.active,
                add_remotes: req.add_remotes,
            },
        )
        .await?;

    tx.audit(
        Entry::new(action::REPO_UPDATED)
            .actor(ctx.user.id)
            .target("repo", repo.slug.clone()),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(repo))
}

/// `GET /api/orgs/{org}/repos/{repo}/leases` — who is in this repo right now.
///
/// The console's answer to "why is my agent waiting?". Read-only, and open to
/// any member **of this repo's team** (or any admin): a lease is a
/// coordination signal, but a team-scoped repo's leases are exactly the kind
/// of team-scoped data `require_visible` exists to keep away from members of
/// other teams.
pub async fn list_leases(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, slug)): Path<(String, String)>,
) -> ApiResult<Json<Vec<Lease>>> {
    let mut tx = state.db.begin(ctx.org.id).await?;
    let repo = tx
        .resolve_repo(&df_core::repos::RepoRef {
            slug: Some(slug),
            remote: None,
        })
        .await?;
    let repo = require_visible(&mut tx, &ctx, repo).await?;
    let leases = tx.list_leases(Some(repo.id)).await?;
    tx.commit().await?;
    Ok(Json(leases))
}
