//! The shared agent-to-agent message channel.
//!
//! Coordination chatter, not a payload transport: "taking the auth refactor,
//! stay out of crates/auth", "job-42 is blocked on the migration, who owns it?".
//! Bodies are bounded because every unread message is re-served on every inbox
//! read by every member — an oversized body is paid for many times over.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::ids::{JobId, OrgId, RepoId, TeamId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Upper bound on a message body, in bytes.
///
/// Bytes rather than chars because the point is the storage and transfer cost,
/// and a multi-byte body can only be shorter in chars, never longer in bytes.
/// 16 KiB is several times the longest plausible hand-off note.
pub const MAX_BODY_LEN: usize = 16 * 1024;

/// Cap on one inbox read, so no caller can force an unbounded read out of a
/// shared server by asking for a huge limit.
pub const INBOX_LIMIT_MAX: i64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "message_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    #[default]
    Note,
    Request,
    Response,
}

/// Who typed a message.
///
/// A rendering hint, not a security claim: a human in the console and their
/// agent session authenticate as the same user, so this cannot be used to
/// distinguish them for authorization. The authoritative `sender_user_id` is
/// always set server-side from the authenticated principal and is never accepted
/// from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "sender_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SenderKind {
    #[default]
    Agent,
    Human,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: i64,
    pub org_id: OrgId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub sender_user_id: UserId,
    pub sender_label: Option<String>,
    pub sender_kind: SenderKind,
    pub recipient_user_id: Option<UserId>,
    pub team_id: Option<TeamId>,
    pub kind: MessageKind,
    pub body: String,
    pub repo_id: Option<RepoId>,
    pub job_id: Option<String>,
    pub in_reply_to: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    pub body: String,
    /// `None` broadcasts to the org (or the team, when `team_id` is set).
    pub recipient_user_id: Option<UserId>,
    pub team_id: Option<TeamId>,
    pub kind: MessageKind,
    pub sender_kind: SenderKind,
    pub sender_label: Option<String>,
    pub repo_id: Option<RepoId>,
    pub job_id: Option<JobId>,
    pub in_reply_to: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct InboxQuery {
    /// Only messages past my cursor that I did not send. Default true.
    pub unread_only: bool,
    pub limit: i64,
    /// Newest first, so a limit-capped read keeps the most recent messages
    /// rather than the oldest. Default false (oldest first, conversational order).
    pub newest_first: bool,
}

impl Default for InboxQuery {
    fn default() -> Self {
        Self {
            unread_only: true,
            limit: 50,
            newest_first: false,
        }
    }
}

const MSG_COLS: &str = "id, org_id, created_at, sender_user_id, sender_label, sender_kind, \
                        recipient_user_id, team_id, kind, body, repo_id, job_id, in_reply_to";

impl Tx<'_> {
    pub async fn send_message(&mut self, sender: UserId, new: NewMessage) -> Result<Message> {
        let body = new.body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("message body must not be empty".into()));
        }
        if body.len() > MAX_BODY_LEN {
            return Err(Error::Invalid(format!(
                "message body is {} bytes; the limit is {MAX_BODY_LEN}",
                body.len()
            )));
        }

        let org = self.org();
        let msg = sqlx::query_as(&format!(
            "INSERT INTO messages (org_id, sender_user_id, sender_label, sender_kind, \
                                   recipient_user_id, team_id, kind, body, repo_id, job_id, \
                                   in_reply_to) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING {MSG_COLS}"
        ))
        .bind(org)
        .bind(sender)
        .bind(new.sender_label.as_deref())
        .bind(new.sender_kind)
        .bind(new.recipient_user_id)
        .bind(new.team_id)
        .bind(new.kind)
        .bind(body)
        .bind(new.repo_id)
        .bind(new.job_id.as_ref().map(|j| j.0.as_str()))
        .bind(new.in_reply_to)
        .fetch_one(self.conn())
        .await?;

        Ok(msg)
    }

    /// Messages visible to `reader`: broadcasts plus anything addressed to them.
    pub async fn inbox(&mut self, reader: UserId, q: &InboxQuery) -> Result<Vec<Message>> {
        let org = self.org();
        let limit = q.limit.clamp(1, INBOX_LIMIT_MAX);

        // Two orderings rather than one parameterized `ORDER BY`: the direction
        // of an ORDER BY cannot be bound as a parameter, and building it by
        // string concatenation from client input is how injection happens.
        let order = if q.newest_first { "DESC" } else { "ASC" };

        let msgs = sqlx::query_as(&format!(
            "SELECT {MSG_COLS} FROM messages m \
             WHERE m.org_id = $1 \
               AND (m.recipient_user_id IS NULL OR m.recipient_user_id = $2) \
               AND (NOT $3 OR ( \
                     m.sender_user_id <> $2 \
                     AND m.id > COALESCE( \
                       (SELECT last_read_id FROM message_cursors \
                        WHERE org_id = $1 AND user_id = $2), 0))) \
             ORDER BY m.id {order} LIMIT $4"
        ))
        .bind(org)
        .bind(reader)
        .bind(q.unread_only)
        .bind(limit)
        .fetch_all(self.conn())
        .await?;

        Ok(msgs)
    }

    /// Advance the read cursor.
    ///
    /// Clamped to the newest existing message id, so an over-large value cannot
    /// suppress messages that have not been written yet. Returns the cursor that
    /// actually landed, which is rarely what a careless caller passed.
    pub async fn ack_messages(&mut self, reader: UserId, up_to: i64) -> Result<i64> {
        let org = self.org();
        let newest: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM messages WHERE org_id = $1")
                .bind(org)
                .fetch_one(self.conn())
                .await?;

        let target = up_to.clamp(0, newest);

        let landed: i64 = sqlx::query_scalar(
            "INSERT INTO message_cursors (org_id, user_id, last_read_id) VALUES ($1,$2,$3) \
             ON CONFLICT (org_id, user_id) DO UPDATE \
               SET last_read_id = GREATEST(message_cursors.last_read_id, EXCLUDED.last_read_id), \
                   updated_at = now() \
             RETURNING last_read_id",
        )
        .bind(org)
        .bind(reader)
        .bind(target)
        .fetch_one(self.conn())
        .await?;

        Ok(landed)
    }

    pub async fn unread_count(&mut self, reader: UserId) -> Result<i64> {
        let org = self.org();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m \
             WHERE m.org_id = $1 \
               AND (m.recipient_user_id IS NULL OR m.recipient_user_id = $2) \
               AND m.sender_user_id <> $2 \
               AND m.id > COALESCE( \
                 (SELECT last_read_id FROM message_cursors \
                  WHERE org_id = $1 AND user_id = $2), 0)",
        )
        .bind(org)
        .bind(reader)
        .fetch_one(self.conn())
        .await?;
        Ok(n)
    }
}
