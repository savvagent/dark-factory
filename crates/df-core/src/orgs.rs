//! Orgs, users, and membership — the control plane.
//!
//! These run on **unpinned** transactions ([`Db::begin_unpinned`]) because they
//! answer the question that must be settled *before* an org can be pinned:
//! "who is this, and which orgs may they act in?". Everything here is reachable
//! only from df-auth and df-web; the MCP tool surface never calls it.

use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use crate::ids::{OrgId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, schemars::JsonSchema,
)]
#[sqlx(type_name = "org_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    /// Owners and admins may manage members, teams, repos, and connections.
    pub fn can_administer(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }

    /// Only owners may change billing or delete the org.
    pub fn can_own(self) -> bool {
        matches!(self, Role::Owner)
    }
}

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
#[sqlx(type_name = "org_plan", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    #[default]
    Free,
    Team,
    Business,
    Enterprise,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Org {
    pub id: OrgId,
    pub slug: String,
    pub name: String,
    pub plan: Plan,
    pub enforce_sso: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub name: Option<String>,
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub org_id: OrgId,
    pub user_id: UserId,
    pub role: Role,
    pub org_slug: String,
    pub org_name: String,
    pub plan: Plan,
}

/// A member of one org, joined with their user record. Flat rather than nested
/// so it maps straight off the query — the console renders exactly these fields.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct OrgMember {
    pub id: UserId,
    pub email: String,
    pub name: Option<String>,
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub role: Role,
    pub joined_at: chrono::DateTime<chrono::Utc>,
}

const ORG_COLS: &str = "id, slug, name, plan, enforce_sso, created_at";
const USER_COLS: &str = "id, email, name, email_verified_at, created_at, disabled_at";

/// An org is addressed by its slug as one URL path segment for the rest of its
/// life (`/api/orgs/{org}/...`), so the same character/length discipline team
/// slugs already get applies here — a slug containing `/` would otherwise be
/// stored and then never addressable again, and an empty-after-trim one would
/// collide with every other org that also trimmed to nothing.
fn validate_org_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_lowercase();
    if slug.is_empty() {
        return Err(Error::Invalid("an org needs a slug".into()));
    }
    if slug.len() > 64 {
        return Err(Error::Invalid(
            "an org slug must be 64 characters or fewer".into(),
        ));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Invalid(format!(
            "org slug {slug:?} may contain only letters, digits, '-' and '_'"
        )));
    }
    Ok(slug)
}

impl Db {
    pub async fn create_org(&self, slug: &str, name: &str) -> Result<Org> {
        let slug = validate_org_slug(slug)?;
        let org = sqlx::query_as(&format!(
            "INSERT INTO orgs (slug, name) VALUES ($1, $2) RETURNING {ORG_COLS}"
        ))
        .bind(&slug)
        .bind(name)
        .fetch_one(self.pool())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Error::Invalid(format!("org slug {slug:?} is taken"))
            }
            _ => Error::Db(e),
        })?;
        Ok(org)
    }

    /// Create an org and add its first owner in one transaction.
    ///
    /// `create_org` followed by a separate `add_member` call would leave a
    /// window — a crash or a failed second statement between them — where the
    /// org exists with no owner at all, the exact invariant the rest of this
    /// module enforces everywhere else. Self-serve org creation always wants
    /// both or neither.
    pub async fn create_org_with_owner(
        &self,
        slug: &str,
        name: &str,
        owner: UserId,
    ) -> Result<Org> {
        let slug = validate_org_slug(slug)?;
        let mut tx = self.begin_unpinned().await?;

        let org: Org = sqlx::query_as(&format!(
            "INSERT INTO orgs (slug, name) VALUES ($1, $2) RETURNING {ORG_COLS}"
        ))
        .bind(&slug)
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Error::Invalid(format!("org slug {slug:?} is taken"))
            }
            _ => Error::Db(e),
        })?;

        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(org.id)
            .bind(owner)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(org)
    }

    pub async fn get_org(&self, id: OrgId) -> Result<Option<Org>> {
        let org = sqlx::query_as(&format!("SELECT {ORG_COLS} FROM orgs WHERE id = $1"))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(org)
    }

    pub async fn get_org_by_slug(&self, slug: &str) -> Result<Option<Org>> {
        let org = sqlx::query_as(&format!(
            "SELECT {ORG_COLS} FROM orgs WHERE lower(slug) = lower($1)"
        ))
        .bind(slug)
        .fetch_optional(self.pool())
        .await?;
        Ok(org)
    }

    /// Create a user, or return the existing one for this email.
    ///
    /// Idempotent because signup, invite acceptance, and federated first-login
    /// all race toward the same row and none of them should fail because
    /// another got there first.
    pub async fn upsert_user(&self, email: &str, name: Option<&str>) -> Result<User> {
        let email = email.trim();
        if email.is_empty() || !email.contains('@') {
            return Err(Error::Invalid(format!("{email:?} is not an email address")));
        }

        if let Some(existing) = self.get_user_by_email(email).await? {
            return Ok(existing);
        }

        let user = sqlx::query_as(&format!(
            "INSERT INTO users (email, name) VALUES ($1, $2) \
             ON CONFLICT (lower(email)) DO UPDATE SET email = users.email \
             RETURNING {USER_COLS}"
        ))
        .bind(email)
        .bind(name)
        .fetch_one(self.pool())
        .await?;

        Ok(user)
    }

    pub async fn get_user(&self, id: UserId) -> Result<Option<User>> {
        let user = sqlx::query_as(&format!("SELECT {USER_COLS} FROM users WHERE id = $1"))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(user)
    }

    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let user = sqlx::query_as(&format!(
            "SELECT {USER_COLS} FROM users WHERE lower(email) = lower($1)"
        ))
        .bind(email)
        .fetch_optional(self.pool())
        .await?;
        Ok(user)
    }

    /// Record that this user has proved control of their email address.
    ///
    /// Idempotent, and it reports which call did the work: a user who clicks the
    /// same verification link twice has done nothing wrong, but only the first
    /// click is worth an audit event.
    pub async fn mark_email_verified(&self, user: UserId) -> Result<bool> {
        let n = sqlx::query(
            "UPDATE users SET email_verified_at = now() \
             WHERE id = $1 AND email_verified_at IS NULL",
        )
        .bind(user)
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn add_member(&self, org: OrgId, user: UserId, role: Role) -> Result<()> {
        sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1,$2,$3) \
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(org)
        .bind(user)
        .bind(role)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn remove_member(&self, org: OrgId, user: UserId) -> Result<()> {
        sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(user)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The user's role in an org, or `None` if they are not a member.
    ///
    /// This is the authorization check every request runs before pinning a
    /// transaction: RLS enforces that a pinned transaction stays inside its org,
    /// but nothing in the database decides *which* org a given user may pin.
    /// That decision is here, and it is the reason a token's org is fixed at
    /// issuance rather than chosen per request.
    pub async fn member_role(&self, org: OrgId, user: UserId) -> Result<Option<Role>> {
        let role =
            sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
                .bind(org)
                .bind(user)
                .fetch_optional(self.pool())
                .await?;
        Ok(role)
    }

    /// How many owners this org has.
    ///
    /// Read-only and unlocked — fine for a display, wrong for a guard. A
    /// caller about to remove or demote an owner wants
    /// [`Tx::count_owners_for_update`] instead, in the same transaction as the
    /// write it is guarding: two concurrent callers reading this method's
    /// answer on separate connections can each see the same count, each pass,
    /// and both writes land, leaving an org with no owner — "a state only a
    /// human with database access can undo".
    pub async fn count_owners(&self, org: OrgId) -> Result<i64> {
        let n = sqlx::query_scalar(
            "SELECT count(*) FROM org_members WHERE org_id = $1 AND role = 'owner'",
        )
        .bind(org)
        .fetch_one(self.pool())
        .await?;
        Ok(n)
    }

    pub async fn list_user_orgs(&self, user: UserId) -> Result<Vec<Membership>> {
        let rows = sqlx::query_as(
            "SELECT m.org_id, m.user_id, m.role, o.slug AS org_slug, o.name AS org_name, o.plan \
             FROM org_members m JOIN orgs o ON o.id = m.org_id \
             WHERE m.user_id = $1 AND o.deleted_at IS NULL \
             ORDER BY o.name",
        )
        .bind(user)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    pub async fn list_org_members(&self, org: OrgId) -> Result<Vec<OrgMember>> {
        let rows = sqlx::query_as(
            "SELECT u.id, u.email, u.name, u.email_verified_at, u.created_at, u.disabled_at, \
                    m.role, m.created_at AS joined_at \
             FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 ORDER BY u.email",
        )
        .bind(org)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

impl Tx<'_> {
    /// Owner rows locked for the rest of this transaction.
    ///
    /// A caller demoting or removing an owner reads this count and, if it
    /// clears the guard, writes the membership change — both inside one
    /// transaction. Without the lock, two concurrent callers (two owners
    /// demoting each other, say) can each read `count == 2` on separate
    /// connections, each pass the guard, and both writes land, leaving the
    /// org with zero owners: "a state only a human with database access can
    /// undo" (see [`Db::count_owners`]). `FOR UPDATE` cannot ride an
    /// aggregate, so this locks the owner rows themselves and returns how
    /// many there were; a second transaction reaching the same rows blocks
    /// here until the first commits or rolls back, then sees the count that
    /// transaction left behind.
    pub async fn count_owners_for_update(&mut self) -> Result<i64> {
        let org = self.org();
        let owners: Vec<(UserId,)> = sqlx::query_as(
            "SELECT user_id FROM org_members WHERE org_id = $1 AND role = 'owner' FOR UPDATE",
        )
        .bind(org)
        .fetch_all(self.conn())
        .await?;
        Ok(owners.len() as i64)
    }

    /// As [`Db::add_member`], pinned to this transaction so a role change can
    /// share a commit with [`Tx::count_owners_for_update`]'s guard.
    pub async fn add_member(&mut self, user: UserId, role: Role) -> Result<()> {
        let org = self.org();
        sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1,$2,$3) \
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(org)
        .bind(user)
        .bind(role)
        .execute(self.conn())
        .await?;
        Ok(())
    }

    /// As [`Db::remove_member`], pinned to this transaction so the guard, the
    /// team cleanup, and the audit entry either all land or none do.
    pub async fn remove_member(&mut self, user: UserId) -> Result<()> {
        let org = self.org();
        sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(user)
            .execute(self.conn())
            .await?;
        Ok(())
    }
}
