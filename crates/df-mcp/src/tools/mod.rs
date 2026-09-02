//! The tool surface.
//!
//! Split by domain so each file stays readable; `rmcp`'s [`ToolRouter`] adds,
//! so the four routers are combined in [`router`].
//!
//! ## Two conventions every tool follows
//!
//! **Naming a repo.** Coordination is anchored on repos, so almost every tool
//! takes an optional `repo` (a registered slug) *or* `remote` (whatever
//! `git remote get-url origin` printed, in any spelling). The agent passes what
//! it has and the server normalizes. When neither resolves, the error lists the
//! registered slugs and points at `register_repo` — it never falls back to a
//! default, because queueing work against a repo nobody meant is a silent,
//! expensive failure and an error is a cheap one.
//!
//! **Output shape.** Every result is a one-field object — `{"job": …}`,
//! `{"jobs": […]}` — carrying `df-core`'s own domain types. See [`out`] for why
//! it is an envelope rather than the bare value, and why the payloads are not
//! mirrored into view structs.

use df_core::ids::RepoId;
use df_core::repos::{Repo, RepoRef};
use df_core::Tx;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::ErrorData;

use crate::server::{Factory, McpResult};

pub mod coord;
pub mod jobs;
pub mod org;
pub mod out;
pub mod repos;

/// Every tool this server exposes.
///
/// `#[tool_router]` generates its constructor as an associated function on the
/// type it is applied to, so these are `Factory::*` rather than module-level
/// functions even though each is written in its own file.
pub fn router() -> ToolRouter<Factory> {
    Factory::repos_router()
        + Factory::jobs_router()
        + Factory::coord_router()
        + Factory::org_router()
}

/// Scope names, matching [`df_auth::oauth::KNOWN_SCOPES`].
pub mod scope {
    pub const JOBS_READ: &str = "jobs:read";
    pub const JOBS_WRITE: &str = "jobs:write";
    pub const REPOS_READ: &str = "repos:read";
    pub const REPOS_WRITE: &str = "repos:write";
    pub const MESSAGES: &str = "messages";
}

/// Resolve a repo the caller named, or fail with the error that lists what is
/// registered.
pub(crate) async fn repo_of(
    tx: &mut Tx<'_>,
    slug: Option<String>,
    remote: Option<String>,
) -> Result<Repo, ErrorData> {
    tx.resolve_repo(&RepoRef { slug, remote }).await.mcp()
}

/// Resolve a repo only if the caller named one.
///
/// For the tools where a repo narrows a query rather than anchoring a write —
/// `list_jobs`, `ready`, `stats`, `list_leases`. Passing nothing means "the
/// whole org", which is a legitimate question; passing something unresolvable
/// is still an error, because a filter that silently matched everything would
/// answer a question the caller did not ask.
pub(crate) async fn maybe_repo_of(
    tx: &mut Tx<'_>,
    slug: Option<String>,
    remote: Option<String>,
) -> Result<Option<RepoId>, ErrorData> {
    let named = slug.as_deref().is_some_and(|s| !s.trim().is_empty())
        || remote.as_deref().is_some_and(|s| !s.trim().is_empty());

    if !named {
        return Ok(None);
    }
    Ok(Some(repo_of(tx, slug, remote).await?.id))
}
