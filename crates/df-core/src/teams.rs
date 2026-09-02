//! Teams — the subdivision inside an org.
//!
//! A team is a visibility scope, not a second tenant boundary. Repos and jobs
//! carry a nullable `team_id`: when it is set, the row belongs to that team;
//! when it is null, the row is org-wide. Nothing here weakens the org boundary —
//! every statement is still `org_id = $1` inside a pinned transaction, and a
//! team id from another org simply does not resolve.
//!
//! **Membership of a team requires membership of the org**, checked here rather
//! than trusted from the caller. `team_members` has its own `org_id` column and
//! its own RLS policy, so a bad insert would be refused by the database — but it
//! would be refused with a constraint error rather than a sentence an admin can
//! act on, and "that person is not in this org yet; invite them first" is the
//! useful answer.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::ids::{OrgId, TeamId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    pub id: TeamId,
    pub org_id: OrgId,
    pub slug: String,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A team member, joined with their user record — the shape the console renders.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    pub user_id: UserId,
    pub email: String,
    pub name: Option<String>,
    pub joined_at: chrono::DateTime<chrono::Utc>,
}

/// Fields a team update may change. `None` leaves a field alone, which is what
/// makes this a PATCH rather than a replace.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamPatch {
    pub slug: Option<String>,
    pub name: Option<String>,
}

const TEAM_COLS: &str = "id, org_id, slug, name, created_at";

/// Slugs appear in URLs and in agent-facing arguments, so they are constrained
/// to something unambiguous rather than normalized silently — a team the admin
/// typed as "Platform Team" that comes back as "platform-team" is a surprise
/// every time they go looking for it.
fn validate_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_lowercase();
    if slug.is_empty() {
        return Err(Error::Invalid("a team needs a slug".into()));
    }
    if slug.len() > 64 {
        return Err(Error::Invalid(
            "a team slug must be 64 characters or fewer".into(),
        ));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Invalid(format!(
            "team slug {slug:?} may contain only letters, digits, '-' and '_'"
        )));
    }
    Ok(slug)
}

impl Tx<'_> {
    pub async fn create_team(&mut self, slug: &str, name: &str) -> Result<Team> {
        let slug = validate_slug(slug)?;
        let name = name.trim();
        let name = if name.is_empty() { &slug } else { name };
        let org = self.org();

        sqlx::query_as(&format!(
            "INSERT INTO teams (org_id, slug, name) VALUES ($1,$2,$3) RETURNING {TEAM_COLS}"
        ))
        .bind(org)
        .bind(&slug)
        .bind(name)
        .fetch_one(self.conn())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => Error::TeamSlugTaken(slug),
            _ => Error::Db(e),
        })
    }

    pub async fn list_teams(&mut self) -> Result<Vec<Team>> {
        let org = self.org();
        let teams = sqlx::query_as(&format!(
            "SELECT {TEAM_COLS} FROM teams WHERE org_id = $1 ORDER BY name"
        ))
        .bind(org)
        .fetch_all(self.conn())
        .await?;
        Ok(teams)
    }

    pub async fn get_team(&mut self, id: TeamId) -> Result<Option<Team>> {
        let org = self.org();
        let team = sqlx::query_as(&format!(
            "SELECT {TEAM_COLS} FROM teams WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
        Ok(team)
    }

    pub async fn get_team_by_slug(&mut self, slug: &str) -> Result<Option<Team>> {
        let org = self.org();
        let team = sqlx::query_as(&format!(
            "SELECT {TEAM_COLS} FROM teams WHERE org_id = $1 AND slug = lower($2)"
        ))
        .bind(org)
        .bind(slug.trim())
        .fetch_optional(self.conn())
        .await?;
        Ok(team)
    }

    /// Resolve a team by slug, or fail naming what is registered.
    ///
    /// The same rule as repo resolution: an unresolvable name is an error that
    /// lists the alternatives, never a silent fallback to "no team", because
    /// "no team" means org-wide and quietly widening a scope is the worst
    /// available answer.
    pub async fn resolve_team(&mut self, slug: &str) -> Result<Team> {
        if let Some(team) = self.get_team_by_slug(slug).await? {
            return Ok(team);
        }
        let known = self
            .list_teams()
            .await?
            .iter()
            .map(|t| t.slug.clone())
            .collect::<Vec<_>>()
            .join(", ");
        Err(Error::TeamNotFound {
            slug: slug.trim().to_string(),
            known: if known.is_empty() {
                "(none yet)".into()
            } else {
                known
            },
        })
    }

    pub async fn update_team(&mut self, id: TeamId, patch: TeamPatch) -> Result<Team> {
        let slug = patch.slug.as_deref().map(validate_slug).transpose()?;
        let org = self.org();

        sqlx::query_as(&format!(
            "UPDATE teams SET slug = COALESCE($3, slug), name = COALESCE($4, name) \
             WHERE org_id = $1 AND id = $2 RETURNING {TEAM_COLS}"
        ))
        .bind(org)
        .bind(id)
        .bind(slug.as_deref())
        .bind(
            patch
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty()),
        )
        .fetch_optional(self.conn())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Error::TeamSlugTaken(slug.unwrap_or_default())
            }
            _ => Error::Db(e),
        })?
        .ok_or_else(|| Error::TeamNotFound {
            slug: id.to_string(),
            known: String::new(),
        })
    }

    /// Delete a team, refusing while anything is still scoped to it.
    ///
    /// The schema would allow this: `repos.team_id`, `jobs.team_id`, and
    /// `messages.team_id` are all `ON DELETE SET NULL`. That is exactly why it
    /// is refused here — a null `team_id` means *org-wide*, so cascading would
    /// silently widen a deliberately narrowed scope, and the admin who deleted
    /// a stale team would have published its repos, job history, and messages
    /// to the whole org without being told. Jobs and messages are checked too,
    /// not just repos: a job keeps its `team_id` after its repo is
    /// unassigned, so team-scoped history can outlive the repo link entirely.
    /// The error names the repos so the fix is obvious: reassign them, then
    /// delete.
    pub async fn delete_team(&mut self, id: TeamId) -> Result<()> {
        let org = self.org();

        let scoped: Vec<String> = sqlx::query_scalar(
            "SELECT slug FROM repos WHERE org_id = $1 AND team_id = $2 ORDER BY slug",
        )
        .bind(org)
        .bind(id)
        .fetch_all(self.conn())
        .await?;

        if !scoped.is_empty() {
            return Err(Error::TeamInUse {
                repos: scoped.join(", "),
            });
        }

        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE org_id = $1 AND team_id = $2")
                .bind(org)
                .bind(id)
                .fetch_one(self.conn())
                .await?;

        if jobs > 0 {
            return Err(Error::TeamInUse {
                repos: format!("{jobs} job(s) still reference this team"),
            });
        }

        let messages: i64 =
            sqlx::query_scalar("SELECT count(*) FROM messages WHERE org_id = $1 AND team_id = $2")
                .bind(org)
                .bind(id)
                .fetch_one(self.conn())
                .await?;

        if messages > 0 {
            return Err(Error::TeamInUse {
                repos: format!("{messages} message(s) still reference this team"),
            });
        }

        let n = sqlx::query("DELETE FROM teams WHERE org_id = $1 AND id = $2")
            .bind(org)
            .bind(id)
            .execute(self.conn())
            .await?
            .rows_affected();

        if n == 0 {
            return Err(Error::TeamNotFound {
                slug: id.to_string(),
                known: String::new(),
            });
        }
        Ok(())
    }

    /// Put an org member on a team. Idempotent — adding twice is not an error,
    /// because the admin's intent ("this person is on this team") is satisfied
    /// either way.
    pub async fn add_team_member(&mut self, team: TeamId, user: UserId) -> Result<()> {
        let org = self.org();

        // The team must be ours. Without this the insert would still be refused
        // — by RLS, on `team_members.org_id` — but with a constraint error
        // rather than a sentence.
        if self.get_team(team).await?.is_none() {
            return Err(Error::TeamNotFound {
                slug: team.to_string(),
                known: String::new(),
            });
        }

        let is_member: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM org_members WHERE org_id = $1 AND user_id = $2)",
        )
        .bind(org)
        .bind(user)
        .fetch_one(self.conn())
        .await?;

        if !is_member {
            return Err(Error::NotAMember(user));
        }

        sqlx::query(
            "INSERT INTO team_members (org_id, team_id, user_id) VALUES ($1,$2,$3) \
             ON CONFLICT (team_id, user_id) DO NOTHING",
        )
        .bind(org)
        .bind(team)
        .bind(user)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    pub async fn remove_team_member(&mut self, team: TeamId, user: UserId) -> Result<()> {
        let org = self.org();
        sqlx::query("DELETE FROM team_members WHERE org_id = $1 AND team_id = $2 AND user_id = $3")
            .bind(org)
            .bind(team)
            .bind(user)
            .execute(self.conn())
            .await?;
        Ok(())
    }

    pub async fn list_team_members(&mut self, team: TeamId) -> Result<Vec<TeamMember>> {
        let org = self.org();
        let rows = sqlx::query_as(
            "SELECT tm.user_id, u.email, u.name, tm.created_at AS joined_at \
             FROM team_members tm JOIN users u ON u.id = tm.user_id \
             WHERE tm.org_id = $1 AND tm.team_id = $2 ORDER BY u.email",
        )
        .bind(org)
        .bind(team)
        .fetch_all(self.conn())
        .await?;
        Ok(rows)
    }

    /// The teams one user belongs to in this org — what decides which
    /// team-scoped repos and jobs they can see.
    pub async fn list_user_teams(&mut self, user: UserId) -> Result<Vec<Team>> {
        let org = self.org();
        let rows = sqlx::query_as(
            "SELECT t.id, t.org_id, t.slug, t.name, t.created_at \
             FROM teams t JOIN team_members tm ON tm.team_id = t.id \
             WHERE t.org_id = $1 AND tm.org_id = $1 AND tm.user_id = $2 ORDER BY t.name",
        )
        .bind(org)
        .bind(user)
        .fetch_all(self.conn())
        .await?;
        Ok(rows)
    }

    /// Remove a user from every team in this org. Called when they leave the
    /// org, so a re-invited person does not silently reappear on their old
    /// teams months later.
    pub async fn remove_from_all_teams(&mut self, user: UserId) -> Result<u64> {
        let org = self.org();
        let n = sqlx::query("DELETE FROM team_members WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(user)
            .execute(self.conn())
            .await?
            .rows_affected();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_constrained_and_lowercased() {
        assert_eq!(validate_slug(" Platform ").unwrap(), "platform");
        assert_eq!(validate_slug("web-ui_2").unwrap(), "web-ui_2");

        for bad in ["", "   ", "platform team", "team/one", "über"] {
            assert!(validate_slug(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(validate_slug(&"a".repeat(65)).is_err());
    }
}
