//! Repos — the coordination anchor.
//!
//! A repo row is what makes an agent's working directory meaningful to the
//! server. The agent passes whatever `git remote get-url origin` gave it; this
//! module turns that into exactly one registered repo, or a clear error.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::ids::{OrgId, RepoId, TeamId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    sqlx::Type,
    schemars::JsonSchema,
)]
#[sqlx(type_name = "repo_provider", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
    Bitbucket,
    #[default]
    Other,
}

impl Provider {
    /// Infer the provider from an already-normalized remote. A hint for display
    /// and for tracker binding defaults — never a gate on anything.
    pub fn from_normalized(normalized: &str) -> Self {
        let host = normalized.split('/').next().unwrap_or_default();
        if host.ends_with("github.com") {
            Provider::Github
        } else if host.ends_with("gitlab.com") || host.starts_with("gitlab.") {
            Provider::Gitlab
        } else if host.ends_with("bitbucket.org") {
            Provider::Bitbucket
        } else {
            Provider::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub id: RepoId,
    pub org_id: OrgId,
    pub slug: String,
    pub name: String,
    pub provider: Provider,
    pub default_branch: String,
    pub team_id: Option<TeamId>,
    pub default_agent_type: Option<String>,
    pub tracker_binding: serde_json::Value,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<UserId>,
}

#[derive(Debug, Clone, Default)]
pub struct NewRepo {
    pub slug: String,
    pub name: Option<String>,
    /// Raw remote URLs in any form. Normalized before storage.
    pub remotes: Vec<String>,
    pub provider: Option<Provider>,
    pub default_branch: Option<String>,
    pub team_id: Option<TeamId>,
    pub default_agent_type: Option<String>,
    pub tracker_binding: Option<serde_json::Value>,
    pub created_by: Option<UserId>,
}

/// How a caller names a repo. Both fields optional; resolution tries them in
/// the documented order and errors rather than guessing.
#[derive(Debug, Clone, Default)]
pub struct RepoRef {
    /// An explicit registered slug. Wins over everything else.
    pub slug: Option<String>,
    /// A raw git remote URL, in any form git would print.
    pub remote: Option<String>,
}

/// A partial update to a repo. Every field is `None` for "leave alone", so a
/// caller that knows about three fields cannot blank the two it has never heard
/// of — which is what a whole-object PUT does to a client written against an
/// older version of the schema.
///
/// The slug is deliberately absent. It is the handle agents type, it appears in
/// every error message listing what is registered, and renaming it silently
/// breaks every skill and script that names the old one. Retiring a repo is
/// [`Tx::set_repo_active`]; a genuine rename is a new registration.
#[derive(Debug, Clone, Default)]
pub struct RepoPatch {
    pub name: Option<String>,
    pub default_branch: Option<String>,
    /// Three states, not two: `None` leaves the team alone, `Some(Some(id))`
    /// moves the repo to that team, and `Some(None)` makes it org-wide.
    ///
    /// A plain `Option<TeamId>` cannot express the last one — "absent" and
    /// "clear it" collapse into the same value — so a repo could be scoped to a
    /// team and never unscoped. That is not a cosmetic gap: `delete_team`
    /// refuses while repos are still scoped to it, so without a way to unassign
    /// them a team becomes undeletable.
    pub team_id: Option<Option<TeamId>>,
    pub default_agent_type: Option<String>,
    pub tracker_binding: Option<serde_json::Value>,
    pub active: Option<bool>,
    /// Raw remote URLs to add. Normalized, and additive — an existing remote is
    /// never removed here, because a remote that has already resolved jobs to
    /// this repo is load-bearing history.
    pub add_remotes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Remote normalization
// ---------------------------------------------------------------------------

/// Reduce any git remote URL to a canonical `host/path` key.
///
/// The same repository is spelled many ways by the same tooling — `git clone`
/// over SSH, a CI checkout over HTTPS, a token-bearing URL in an automation, a
/// mirror with an explicit port. All of them must land on one row, because a
/// team whose agents disagree about which repo they are in is not coordinated.
///
/// Handles, in order: scp-style `git@host:owner/repo.git`, any `scheme://`
/// form, embedded credentials, explicit ports, trailing `.git`, trailing and
/// duplicated slashes, and host/path case.
///
/// Case: the whole key is lowercased. GitHub, GitLab and Bitbucket all treat
/// owner/repo case-insensitively, so folding it merges spellings that are
/// genuinely the same repo. A self-hosted server with case-sensitive paths
/// could in principle host both `Acme/API` and `acme/api`; that is not a
/// configuration anyone runs, and the cost of getting it wrong (two rows that
/// should be one) is much worse than the cost of this assumption.
///
/// Input that is not URL-shaped (a bare local path) is passed through with the
/// same trailing-`.git` and case treatment, so local-only repos still resolve
/// consistently.
pub fn normalize_remote(raw: &str) -> String {
    let mut s = raw.trim();

    // Strip a trailing slash before anything else so `…/api/` and `…/api` agree.
    while let Some(t) = s.strip_suffix('/') {
        s = t;
    }

    let mut rest: String = if let Some((_scheme, after)) = s.split_once("://") {
        after.to_string()
    } else if let Some((before_colon, after_colon)) = s.split_once(':') {
        // scp-style `[user@]host:path` — but only when the part after the colon
        // is not a port number followed by a path, and the part before contains
        // no slash (otherwise it is a path that merely contains a colon).
        if !before_colon.contains('/') && !after_colon.starts_with("//") {
            format!("{before_colon}/{after_colon}")
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    };

    // Drop embedded credentials (`user:token@host`). Only in the authority
    // segment: an `@` later in the path is legitimate.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    if let Some(at) = rest[..authority_end].rfind('@') {
        rest.replace_range(..=at, "");
    }

    // Drop an explicit port. Same restriction to the authority segment, and only
    // when what follows the colon is actually numeric — `host:owner/repo` was
    // already rewritten above, but a defensive check keeps a path like
    // `host/a:b` intact.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    if let Some(colon) = rest[..authority_end].find(':') {
        let port = &rest[colon + 1..authority_end];
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            rest.replace_range(colon..authority_end, "");
        }
    }

    // Trailing `.git`.
    if let Some(t) = rest.strip_suffix(".git") {
        rest = t.to_string();
    }

    // Collapse duplicate slashes and drop empty segments.
    let joined = rest
        .split('/')
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("/");

    joined.to_lowercase()
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

const REPO_COLS: &str = "id, org_id, slug, name, provider, default_branch, team_id, \
                         default_agent_type, tracker_binding, active, created_at, created_by";

impl Tx<'_> {
    /// Register a repo and its remotes.
    pub async fn register_repo(&mut self, new: NewRepo) -> Result<Repo> {
        if new.slug.trim().is_empty() {
            return Err(Error::Invalid("repo slug must not be empty".into()));
        }

        let normalized: Vec<String> = new
            .remotes
            .iter()
            .map(|r| normalize_remote(r))
            .filter(|r| !r.is_empty())
            .collect();

        // Infer the provider from the first remote when the caller didn't say.
        let provider = new.provider.unwrap_or_else(|| {
            normalized
                .first()
                .map(|n| Provider::from_normalized(n))
                .unwrap_or_default()
        });

        let org = self.org();
        let name = new.name.unwrap_or_else(|| new.slug.clone());

        let repo: Repo = sqlx::query_as(&format!(
            "INSERT INTO repos (org_id, slug, name, provider, default_branch, team_id, \
                                default_agent_type, tracker_binding, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {REPO_COLS}"
        ))
        .bind(org)
        .bind(&new.slug)
        .bind(&name)
        .bind(provider)
        .bind(new.default_branch.as_deref().unwrap_or("main"))
        .bind(new.team_id)
        .bind(new.default_agent_type.as_deref())
        .bind(new.tracker_binding.unwrap_or_else(|| serde_json::json!({})))
        .bind(new.created_by)
        .fetch_one(self.conn())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Error::RepoSlugTaken(new.slug.clone())
            }
            _ => Error::Db(e),
        })?;

        for n in &normalized {
            self.attach_remote(repo.id, n).await?;
        }

        Ok(repo)
    }

    /// Attach an already-normalized remote to a repo.
    async fn attach_remote(&mut self, repo_id: RepoId, normalized: &str) -> Result<()> {
        let org = self.org();
        let res = sqlx::query(
            "INSERT INTO repo_remotes (org_id, repo_id, normalized) VALUES ($1, $2, $3) \
             ON CONFLICT (org_id, normalized) DO NOTHING",
        )
        .bind(org)
        .bind(repo_id)
        .bind(normalized)
        .execute(self.conn())
        .await?;

        // A no-op insert means this remote is already claimed. Silently letting
        // that pass would leave the caller believing their repo is reachable by
        // a remote that actually resolves to a different one.
        if res.rows_affected() == 0 {
            let owner: Option<String> = sqlx::query_scalar(
                "SELECT r.slug FROM repo_remotes m JOIN repos r ON r.id = m.repo_id \
                 WHERE m.org_id = $1 AND m.normalized = $2",
            )
            .bind(org)
            .bind(normalized)
            .fetch_optional(self.conn())
            .await?;

            if let Some(slug) = owner {
                let same = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM repo_remotes \
                     WHERE org_id = $1 AND normalized = $2 AND repo_id = $3)",
                )
                .bind(org)
                .bind(normalized)
                .bind(repo_id)
                .fetch_one(self.conn())
                .await?;

                if !same {
                    return Err(Error::RemoteTaken(normalized.to_string(), slug));
                }
            }
        }
        Ok(())
    }

    /// Add a remote to an existing repo, normalizing it first.
    pub async fn add_remote(&mut self, repo_id: RepoId, raw_remote: &str) -> Result<String> {
        let normalized = normalize_remote(raw_remote);
        if normalized.is_empty() {
            return Err(Error::Invalid(format!(
                "{raw_remote:?} does not look like a git remote"
            )));
        }
        self.attach_remote(repo_id, &normalized).await?;
        Ok(normalized)
    }

    pub async fn list_repos(&mut self, include_inactive: bool) -> Result<Vec<Repo>> {
        let org = self.org();
        let repos = sqlx::query_as(&format!(
            "SELECT {REPO_COLS} FROM repos \
             WHERE org_id = $1 AND ($2 OR active) ORDER BY slug"
        ))
        .bind(org)
        .bind(include_inactive)
        .fetch_all(self.conn())
        .await?;
        Ok(repos)
    }

    pub async fn get_repo_by_slug(&mut self, slug: &str) -> Result<Option<Repo>> {
        let org = self.org();
        let repo = sqlx::query_as(&format!(
            "SELECT {REPO_COLS} FROM repos WHERE org_id = $1 AND slug = $2"
        ))
        .bind(org)
        .bind(slug)
        .fetch_optional(self.conn())
        .await?;
        Ok(repo)
    }

    pub async fn get_repo(&mut self, id: RepoId) -> Result<Option<Repo>> {
        let org = self.org();
        let repo = sqlx::query_as(&format!(
            "SELECT {REPO_COLS} FROM repos WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
        Ok(repo)
    }

    /// Resolve a [`RepoRef`] to exactly one repo.
    ///
    /// Order: explicit slug → normalized remote match. There is deliberately no
    /// "org default" fallback — an unresolvable repo raises
    /// [`Error::RepoUnresolved`] listing the registered slugs, whichever way it
    /// was named. Queueing work against a repo the agent did not mean is a
    /// silent, expensive failure; an error the agent can read and act on is a
    /// cheap one.
    pub async fn resolve_repo(&mut self, r: &RepoRef) -> Result<Repo> {
        if let Some(slug) = r.slug.as_deref().filter(|s| !s.trim().is_empty()) {
            if let Some(repo) = self.get_repo_by_slug(slug).await? {
                return Ok(repo);
            }
            // An explicit slug that misses stops here — it does **not** fall
            // through to the remote. A caller that named a repo and also
            // supplied its working directory's remote would otherwise get the
            // checkout it happens to be in whenever it typos the name, which is
            // the silent guess this function exists to refuse.
            //
            // The error still lists what is registered. A typo'd slug is the
            // commonest way to reach it and the one the caller can actually fix
            // from the answer, so answering "repo not found: apo" and stopping
            // there makes them go and look the name up somewhere else.
            return Err(self.unresolved(slug).await?);
        }

        if let Some(remote) = r.remote.as_deref().filter(|s| !s.trim().is_empty()) {
            let normalized = normalize_remote(remote);
            let org = self.org();
            let found: Option<Repo> = sqlx::query_as(&format!(
                "SELECT {} FROM repos r JOIN repo_remotes m ON m.repo_id = r.id \
                 WHERE r.org_id = $1 AND m.normalized = $2",
                REPO_COLS
                    .split(", ")
                    .map(|c| format!("r.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .bind(org)
            .bind(&normalized)
            .fetch_optional(self.conn())
            .await?;

            if let Some(repo) = found {
                return Ok(repo);
            }
        }

        let attempted = r
            .slug
            .clone()
            .or_else(|| r.remote.clone())
            .unwrap_or_else(|| "(nothing supplied)".into());

        Err(self.unresolved(&attempted).await?)
    }

    /// The "could not resolve" error, with the registered slugs in it.
    ///
    /// Returns `Result<Error>` rather than `Error` because listing the repos is
    /// itself a query: a database failure while composing an error message is a
    /// database failure, and reporting it as "no such repo" would send someone
    /// looking for a typo that is not there.
    async fn unresolved(&mut self, attempted: &str) -> Result<Error> {
        let known = self
            .list_repos(false)
            .await?
            .into_iter()
            .map(|r| r.slug)
            .collect::<Vec<_>>();

        Ok(Error::RepoUnresolved {
            attempted: attempted.to_string(),
            known: if known.is_empty() {
                "none registered yet".into()
            } else {
                known.join(", ")
            },
        })
    }

    /// Apply a partial update.
    ///
    /// `COALESCE($n, column)` per field: an omitted field keeps its stored
    /// value rather than being overwritten with a default. New remotes are
    /// attached after the update and go through the same conflict check as
    /// registration, so a remote already claimed by a sibling repo is an error
    /// naming that repo rather than a silent re-point.
    pub async fn update_repo(&mut self, id: RepoId, patch: RepoPatch) -> Result<Repo> {
        let org = self.org();

        let repo: Repo = sqlx::query_as(&format!(
            "UPDATE repos SET name = COALESCE($3, name), \
                    default_branch = COALESCE($4, default_branch), \
                    team_id = CASE WHEN $5 THEN $6 ELSE team_id END, \
                    default_agent_type = COALESCE($7, default_agent_type), \
                    tracker_binding = COALESCE($8, tracker_binding), \
                    active = COALESCE($9, active) \
             WHERE org_id = $1 AND id = $2 RETURNING {REPO_COLS}"
        ))
        .bind(org)
        .bind(id)
        .bind(patch.name.as_deref())
        .bind(patch.default_branch.as_deref())
        // COALESCE cannot express "set this to NULL", so the team is written
        // through an explicit "was it named?" flag instead.
        .bind(patch.team_id.is_some())
        .bind(patch.team_id.flatten())
        .bind(patch.default_agent_type.as_deref())
        .bind(patch.tracker_binding.as_ref())
        .bind(patch.active)
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| Error::RepoNotFound(id.to_string()))?;

        for raw in &patch.add_remotes {
            self.add_remote(repo.id, raw).await?;
        }

        // Re-read only when remotes changed nothing about the row itself — they
        // do not, so the UPDATE's RETURNING is still accurate.
        Ok(repo)
    }

    pub async fn set_repo_active(&mut self, id: RepoId, active: bool) -> Result<()> {
        let org = self.org();
        sqlx::query("UPDATE repos SET active = $3 WHERE org_id = $1 AND id = $2")
            .bind(org)
            .bind(id)
            .bind(active)
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spelling in each group must collapse to the group's key. This is
    /// the test that keeps two agents in the same repo from believing they are
    /// in different ones.
    #[test]
    fn remote_spellings_collapse_to_one_key() {
        let groups: &[(&str, &[&str])] = &[
            (
                "github.com/acme/api",
                &[
                    "git@github.com:acme/api.git",
                    "git@github.com:acme/api",
                    "https://github.com/acme/api.git",
                    "https://github.com/acme/api",
                    "https://github.com/acme/api/",
                    "ssh://git@github.com/acme/api.git",
                    "ssh://git@github.com:22/acme/api.git",
                    "git://github.com/acme/api.git",
                    "https://x-access-token:ghs_secret@github.com/acme/api.git",
                    "https://GitHub.com/Acme/API.git",
                    "  https://github.com/acme/api.git  ",
                ],
            ),
            (
                "gitlab.example.com/group/sub/proj",
                &[
                    "git@gitlab.example.com:group/sub/proj.git",
                    "https://gitlab.example.com/group/sub/proj",
                    "https://gitlab.example.com:8443/group/sub/proj.git",
                ],
            ),
        ];

        for (expected, spellings) in groups {
            for s in *spellings {
                assert_eq!(
                    normalize_remote(s),
                    *expected,
                    "{s:?} should normalize to {expected:?}"
                );
            }
        }
    }

    #[test]
    fn distinct_repos_stay_distinct() {
        // Normalization must not be so aggressive that different repos merge.
        let distinct = [
            "git@github.com:acme/api.git",
            "git@github.com:acme/web.git",
            "git@github.com:other/api.git",
            "git@gitlab.com:acme/api.git",
        ];
        let keys: std::collections::HashSet<_> =
            distinct.iter().map(|s| normalize_remote(s)).collect();
        assert_eq!(
            keys.len(),
            distinct.len(),
            "collapsed distinct repos: {keys:?}"
        );
    }

    /// Local repos (no remote configured, or a filesystem remote) still have to
    /// resolve consistently — and git prints the same local repo both with and
    /// without its `.git` directory, so those two spellings must collapse.
    #[test]
    fn local_paths_normalize_consistently() {
        assert_eq!(normalize_remote("/home/rob/dev/api"), "home/rob/dev/api");
        assert_eq!(normalize_remote("/home/rob/dev/api/"), "home/rob/dev/api");
        assert_eq!(
            normalize_remote("/home/rob/dev/api/.git"),
            "home/rob/dev/api"
        );
        assert_eq!(
            normalize_remote("file:///home/rob/dev/api.git"),
            "home/rob/dev/api"
        );
    }

    #[test]
    fn provider_inference() {
        assert_eq!(
            Provider::from_normalized("github.com/acme/api"),
            Provider::Github
        );
        assert_eq!(
            Provider::from_normalized("bitbucket.org/acme/api"),
            Provider::Bitbucket
        );
        assert_eq!(
            Provider::from_normalized("git.internal.corp/acme/api"),
            Provider::Other
        );
    }
}
