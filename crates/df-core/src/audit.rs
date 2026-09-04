//! The audit trail.
//!
//! Two entry points, matching the two transaction kinds:
//!
//! - [`Tx::audit`] for anything that happens inside an org — role changes, PAT
//!   mints, repo registration, tracker connections. Written in the *same*
//!   transaction as the change itself, so an action and its record commit or
//!   abort together and the trail can never disagree with reality.
//! - [`Db::audit_global`] for events that precede any org context: login
//!   attempts, passkey enrollment.
//!
//! Actions are dotted and stable because they are queried by prefix and because
//! they end up in customers' SIEM exports. Renaming one is a breaking change.

use crate::db::{Db, Tx};
use crate::error::Result;
use crate::ids::{OrgId, UserId};
use serde::Serialize;
use sqlx::FromRow;

/// Stable action names. Use these constants rather than string literals at call
/// sites — a typo in a literal produces an event nobody will ever find.
pub mod action {
    // Authentication (usually global: no org context yet).
    pub const LOGIN_SUCCEEDED: &str = "auth.login.succeeded";
    pub const LOGIN_FAILED: &str = "auth.login.failed";
    pub const LOGOUT: &str = "auth.logout";
    pub const TOTP_ENROLLED: &str = "auth.totp.enrolled";
    pub const TOTP_RESET: &str = "auth.totp.reset";
    pub const RECOVERY_CODE_USED: &str = "auth.recovery_code.used";
    pub const MAGIC_LINK_SENT: &str = "auth.magic_link.sent";
    pub const MAGIC_LINK_CONSUMED: &str = "auth.magic_link.consumed";
    pub const EMAIL_VERIFIED: &str = "auth.email.verified";

    // OAuth / tokens (org-scoped: the org is bound at authorization time).
    pub const CLIENT_REGISTERED: &str = "oauth.client.registered";
    pub const AUTHORIZATION_GRANTED: &str = "oauth.authorization.granted";
    pub const TOKEN_ISSUED: &str = "oauth.token.issued";
    pub const TOKEN_REFRESHED: &str = "oauth.token.refreshed";
    pub const TOKEN_REVOKED: &str = "oauth.token.revoked";
    /// A replayed refresh token. Treated as theft: the whole chain is revoked.
    /// This is the highest-signal line in the table — alert on it.
    pub const REFRESH_REUSE_DETECTED: &str = "oauth.refresh.reuse_detected";
    pub const PAT_MINTED: &str = "oauth.pat.minted";
    pub const PAT_REVOKED: &str = "oauth.pat.revoked";

    // Org administration.
    pub const MEMBER_INVITED: &str = "org.member.invited";
    pub const MEMBER_JOINED: &str = "org.member.joined";
    pub const MEMBER_ROLE_CHANGED: &str = "org.member.role_changed";
    pub const MEMBER_REMOVED: &str = "org.member.removed";
    pub const IDP_CONNECTED: &str = "org.idp.connected";
    pub const IDP_DISCONNECTED: &str = "org.idp.disconnected";
    pub const DOMAIN_CLAIMED: &str = "org.domain.claimed";
    pub const DOMAIN_VERIFIED: &str = "org.domain.verified";
    pub const PLAN_CHANGED: &str = "org.plan.changed";

    // Resources.
    pub const REPO_REGISTERED: &str = "repo.registered";
    pub const REPO_UPDATED: &str = "repo.updated";
    pub const TRACKER_CONNECTED: &str = "tracker.connected";
    pub const TRACKER_DISCONNECTED: &str = "tracker.disconnected";
}

/// One recorded event. Built with the fluent constructors rather than a struct
/// literal so adding a field later does not break every call site.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub actor_user_id: Option<UserId>,
    pub actor_label: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub detail: Option<serde_json::Value>,
}

impl Entry {
    pub fn new(action: &str) -> Self {
        Self {
            action: action.to_string(),
            ..Default::default()
        }
    }

    pub fn actor(mut self, user: UserId) -> Self {
        self.actor_user_id = Some(user);
        self
    }

    pub fn actor_label(mut self, label: impl Into<String>) -> Self {
        self.actor_label = Some(label.into());
        self
    }

    pub fn target(mut self, kind: &str, id: impl Into<String>) -> Self {
        self.target_type = Some(kind.to_string());
        self.target_id = Some(id.into());
        self
    }

    /// Caller IP and user agent, as seen at the HTTP boundary.
    pub fn from_request(mut self, ip: Option<&str>, user_agent: Option<&str>) -> Self {
        self.ip = ip.map(str::to_string);
        self.user_agent = user_agent.map(str::to_string);
        self
    }

    /// Extra context. **Never put a secret, token, or credential here** — org
    /// admins read this table in the console.
    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: i64,
    pub org_id: Option<OrgId>,
    pub actor_user_id: Option<UserId>,
    pub actor_label: Option<String>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const AUDIT_COLS: &str = "id, org_id, actor_user_id, actor_label, action, target_type, \
                          target_id, ip, user_agent, detail, created_at";

const INSERT_SQL: &str = "INSERT INTO audit_events \
     (org_id, actor_user_id, actor_label, action, target_type, target_id, ip, user_agent, detail) \
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)";

impl Tx<'_> {
    /// Record an org-scoped event **in the same transaction as the change it
    /// describes**. If the change rolls back, so does its record.
    pub async fn audit(&mut self, e: Entry) -> Result<()> {
        let org = self.org();
        sqlx::query(INSERT_SQL)
            .bind(org)
            .bind(e.actor_user_id)
            .bind(e.actor_label.as_deref())
            .bind(&e.action)
            .bind(e.target_type.as_deref())
            .bind(e.target_id.as_deref())
            .bind(e.ip.as_deref())
            .bind(e.user_agent.as_deref())
            .bind(e.detail.unwrap_or_else(|| serde_json::json!({})))
            .execute(self.conn())
            .await?;
        Ok(())
    }

    /// Read the org's audit trail, newest first. Powers the console's security
    /// page and any customer SIEM export.
    pub async fn audit_trail(
        &mut self,
        action_prefix: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AuditEvent>> {
        let org = self.org();
        let events = sqlx::query_as(&format!(
            "SELECT {AUDIT_COLS} FROM audit_events \
             WHERE org_id = $1 AND ($2::text IS NULL OR action LIKE $2 || '%') \
             ORDER BY created_at DESC, id DESC LIMIT $3"
        ))
        .bind(org)
        .bind(action_prefix)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.conn())
        .await?;
        Ok(events)
    }
}

impl Db {
    /// Record an event with no org context — a login attempt, a passkey
    /// enrollment.
    ///
    /// Best-effort by design: this returns `Result`, but callers on the failed-
    /// login path should log an error and continue rather than turning an audit
    /// write failure into an authentication outage. Losing one audit row is bad;
    /// refusing every login because the audit table is unavailable is worse.
    pub async fn audit_global(&self, e: Entry) -> Result<()> {
        sqlx::query(INSERT_SQL)
            .bind(Option::<OrgId>::None)
            .bind(e.actor_user_id)
            .bind(e.actor_label.as_deref())
            .bind(&e.action)
            .bind(e.target_type.as_deref())
            .bind(e.target_id.as_deref())
            .bind(e.ip.as_deref())
            .bind(e.user_agent.as_deref())
            .bind(e.detail.unwrap_or_else(|| serde_json::json!({})))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Record an org-scoped event from the control plane, where no tenant
    /// transaction is open (e.g. membership changes made during signup).
    pub async fn audit_for_org(&self, org: OrgId, e: Entry) -> Result<()> {
        sqlx::query(INSERT_SQL)
            .bind(org)
            .bind(e.actor_user_id)
            .bind(e.actor_label.as_deref())
            .bind(&e.action)
            .bind(e.target_type.as_deref())
            .bind(e.target_id.as_deref())
            .bind(e.ip.as_deref())
            .bind(e.user_agent.as_deref())
            .bind(e.detail.unwrap_or_else(|| serde_json::json!({})))
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
