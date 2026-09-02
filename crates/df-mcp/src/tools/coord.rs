//! Coordination tools — leases, the message channel, and change notification.
//!
//! These are what make a queue into coordination. Leases answer "is anyone else
//! in this branch"; messages let agents hand off context a job field cannot
//! hold; `watch` lets an agent sit still until something happens instead of
//! asking every few seconds.

use std::time::Duration;

use df_core::ids::JobId;
use df_core::messages::{InboxQuery, MessageKind, NewMessage, SenderKind};
use df_core::watch::Outcome;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{maybe_repo_of, out, repo_of, scope};
use crate::server::{Factory, McpResult};

/// Default long-poll duration, and the ceiling.
///
/// Thirty seconds is short enough to sit inside every load balancer and proxy
/// idle timeout that agents connect through, and long enough that an idle agent
/// makes two calls a minute rather than sixty. The ceiling is a minute for the
/// same reason: a longer poll is not more efficient if an intermediary silently
/// severs it and the agent has to reconnect anyway.
const WATCH_DEFAULT_SECS: u64 = 30;
const WATCH_MAX_SECS: u64 = 60;

/// Properties of the two constants above rather than of any code path, so they
/// are checked when the crate compiles. A poll longer than a minute is not more
/// efficient — it is severed by an intermediary and reconnected anyway.
const _: () = assert!(WATCH_DEFAULT_SECS <= WATCH_MAX_SECS);
const _: () = assert!(WATCH_MAX_SECS <= 60);

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquireLeaseArgs {
    /// The branch you are about to work on.
    pub branch: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// How you want to appear to teammates who run list_leases, for example
    /// "api-agent@ci-7".
    #[serde(default)]
    pub agent: Option<String>,
    /// The job this lease is for, if there is one.
    #[serde(default)]
    pub job: Option<String>,
    /// How long to hold it before it expires. Defaults to 15 minutes, capped
    /// at 4 hours. Renew rather than asking for a long one: a lease that
    /// outlives a crashed agent blocks the branch for everyone.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RenewLeaseArgs {
    /// The lease id returned by acquire_lease.
    pub lease: String,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseLeaseArgs {
    /// The lease id returned by acquire_lease.
    pub lease: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeArgs {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageArgs {
    /// What you want to say.
    pub body: String,
    /// Email address of one teammate to send this to privately. Omit to
    /// broadcast to everyone in the organization.
    #[serde(default)]
    pub to: Option<String>,
    /// "note" (default), "request" when you need an answer, or "response".
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// The job this is about, if any.
    #[serde(default)]
    pub job: Option<String>,
    /// The id of the message you are replying to.
    #[serde(default)]
    pub in_reply_to: Option<i64>,
    /// How you want to be identified, for example "api-agent@ci-7".
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InboxArgs {
    /// Only messages you have not acknowledged and did not send. Defaults to
    /// true.
    #[serde(default = "default_true")]
    pub unread_only: bool,
    /// Maximum messages to return. Defaults to 50, capped at 200.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Newest first. Defaults to false, which is conversational order.
    #[serde(default)]
    pub newest_first: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AckMessagesArgs {
    /// The highest message id you have read. Clamped to the newest message
    /// that exists, so an over-large value cannot hide messages not yet
    /// written.
    pub up_to: i64,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchArgs {
    /// How long to wait before giving up and returning "timeout". Defaults to
    /// 30 seconds, capped at 60.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

/// Parse the caller's message kind.
fn message_kind(raw: Option<&str>) -> Result<MessageKind, ErrorData> {
    match raw.map(str::trim) {
        None | Some("") | Some("note") => Ok(MessageKind::Note),
        Some("request") => Ok(MessageKind::Request),
        Some("response") => Ok(MessageKind::Response),
        Some(other) => Err(ErrorData::invalid_params(
            format!("unknown message kind {other:?}; expected note, request or response"),
            None,
        )),
    }
}

#[tool_router(router = coord_router, vis = "pub(crate)")]
impl Factory {
    #[tool(
        name = "acquire_lease",
        description = "Announce that you are working on a branch of a repository, so other \
                       agents can see it and go elsewhere. Take one before you start editing \
                       and renew it while you work. If someone already holds it the error \
                       names them and says when it expires, so you can wait, message them, or \
                       pick different work. Leases are advisory: the server cannot see your \
                       git operations, so this makes collisions visible rather than impossible."
    )]
    pub async fn acquire_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AcquireLeaseArgs>,
    ) -> Result<Json<out::LeaseOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let job = args.job.map(JobId::from);
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "acquire_lease").await?;
        let repo = repo_of(&mut tx, args.repo, args.remote).await?;
        let lease = tx
            .acquire_lease(
                repo.id,
                &args.branch,
                caller.user_id,
                args.agent.as_deref(),
                job.as_ref(),
                args.ttl_seconds,
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::LeaseOut { lease }))
    }

    #[tool(
        name = "renew_lease",
        description = "Extend a lease you hold, before it expires. Renew on a cadence \
                       comfortably shorter than the TTL: if it lapses, another agent may take \
                       the branch while you are still in it."
    )]
    pub async fn renew_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RenewLeaseArgs>,
    ) -> Result<Json<out::LeaseOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = lease_id(&args.lease)?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "renew_lease").await?;
        let lease = tx
            .renew_lease(id, caller.user_id, args.ttl_seconds)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::LeaseOut { lease }))
    }

    #[tool(
        name = "release_lease",
        description = "Give up a lease you hold, freeing the branch immediately instead of \
                       waiting for it to expire. Do this as soon as you stop working."
    )]
    pub async fn release_lease(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ReleaseLeaseArgs>,
    ) -> Result<Json<out::ReleasedOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = lease_id(&args.lease)?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "release_lease").await?;
        tx.release_lease(id, caller.user_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::ReleasedOut { released: id }))
    }

    #[tool(
        name = "list_leases",
        description = "Who is working where right now: the live leases across the organization \
                       or one repository, with the holder, the branch, and when each expires. \
                       Expired leases are not listed."
    )]
    pub async fn list_leases(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::LeasesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_leases").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let leases = tx.list_leases(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::LeasesOut { leases }))
    }

    #[tool(
        name = "send_message",
        description = "Post a note to the team's shared channel, or privately to one teammate \
                       by email address. Use it for hand-offs and questions that do not fit in \
                       a job field — 'I have left the migration half-applied on this branch' is \
                       exactly the kind of thing that belongs here."
    )]
    pub async fn send_message(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<Json<out::MessageOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let kind = message_kind(args.kind.as_deref())?;
        let recipient = match args.to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(email) => Some(self.member_by_email(&caller, email).await?),
            None => None,
        };

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "send_message").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let message = tx
            .send_message(
                caller.user_id,
                NewMessage {
                    body: args.body,
                    recipient_user_id: recipient,
                    kind,
                    // Always `Agent` from this surface. A human writing in the
                    // console is the same user authenticating the same way, so
                    // this is a rendering hint and never an authorization
                    // claim — which is why it is set here rather than accepted
                    // from the caller.
                    sender_kind: SenderKind::Agent,
                    sender_label: args.agent,
                    repo_id,
                    job_id: args.job.map(JobId::from),
                    in_reply_to: args.in_reply_to,
                    ..Default::default()
                },
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::MessageOut { message }))
    }

    #[tool(
        name = "inbox",
        description = "Read messages addressed to you or broadcast to the organization. \
                       Defaults to unread only, excluding your own. Call ack_messages with the \
                       highest id you have read so the next call does not repeat them."
    )]
    pub async fn inbox(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<InboxArgs>,
    ) -> Result<Json<out::MessagesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let q = InboxQuery {
            unread_only: args.unread_only,
            limit: args.limit.unwrap_or(50),
            newest_first: args.newest_first,
        };

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "inbox").await?;
        let messages = tx.inbox(caller.user_id, &q).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::MessagesOut { messages }))
    }

    #[tool(
        name = "ack_messages",
        description = "Move your read cursor to a message id, so those messages stop appearing \
                       in your unread inbox. Returns the cursor that actually landed, which is \
                       clamped to the newest message that exists."
    )]
    pub async fn ack_messages(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AckMessagesArgs>,
    ) -> Result<Json<out::CursorOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "ack_messages").await?;
        let cursor = tx.ack_messages(caller.user_id, args.up_to).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::CursorOut { cursor }))
    }

    #[tool(
        name = "unread_count",
        description = "How many messages are waiting for you. Cheap enough to call before \
                       deciding whether reading the inbox is worth it."
    )]
    pub async fn unread_count(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::UnreadOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::MESSAGES).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "unread_count").await?;
        let unread = tx.unread_count(caller.user_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::UnreadOut { unread }))
    }

    #[tool(
        name = "watch",
        description = "Block until something changes in your organization — a job, a lease, or \
                       a message — then return \"changed\". Returns \"timeout\" if nothing \
                       happens first. Use this instead of calling list_jobs in a loop: it is \
                       the difference between reacting in a second and hammering the server. \
                       Messages you sent yourself never wake you. It tells you only that \
                       something changed, so refetch what you care about afterwards."
    )]
    pub async fn watch(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<WatchArgs>,
    ) -> Result<Json<out::WatchOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let secs = args
            .timeout_seconds
            .unwrap_or(WATCH_DEFAULT_SECS)
            .clamp(1, WATCH_MAX_SECS);

        // Metered in a transaction of its own, committed before the wait
        // starts. Every other tool records inside the transaction doing the
        // work, which is what makes a failed call unbilled — but this call's
        // "work" is to sit still for thirty seconds, and holding a transaction
        // open for that long would pin a pool connection per idle agent and
        // exhaust the pool long before the queue was ever busy. `watch` is
        // free, so there is no billing consequence to recording it up front.
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "watch").await?;
        tx.commit().await.mcp()?;

        let outcome = self
            .watcher()
            .wait(
                caller.org_id,
                Some(caller.user_id),
                Duration::from_secs(secs),
            )
            .await;

        Ok(Json(out::WatchOut {
            outcome: match outcome {
                Outcome::Changed => out::WatchOutcome::Changed,
                Outcome::Timeout => out::WatchOutcome::Timeout,
            },
            waited_seconds: secs,
        }))
    }
}

/// Parse a lease id, refusing anything that is not a UUID before it reaches a
/// query.
fn lease_id(raw: &str) -> Result<uuid::Uuid, ErrorData> {
    raw.trim().parse().map_err(|_| {
        ErrorData::invalid_params(
            format!("{raw:?} is not a lease id; pass the id returned by acquire_lease"),
            None,
        )
    })
}

impl Factory {
    /// Resolve an email address to a member of the caller's own org.
    ///
    /// `users` is a global table — one human, one row, however many orgs they
    /// belong to — so an email lookup alone crosses the tenant boundary. The
    /// membership check is what keeps it inside: without it, this tool would
    /// both deliver messages to strangers and answer "does this address have a
    /// dark-factory account?" for anyone who asked.
    ///
    /// The failure says "no member of this organization", never "no such user",
    /// for the same reason the login form is careful: the two are
    /// indistinguishable to the caller and only one of them is safe to confirm.
    async fn member_by_email(
        &self,
        caller: &df_auth::tokens::Principal,
        email: &str,
    ) -> Result<df_core::ids::UserId, ErrorData> {
        let not_a_member = || {
            ErrorData::invalid_params(
                format!(
                    "no member of this organization has the address {email:?}; \
                     omit `to` to broadcast to everyone instead"
                ),
                Some(serde_json::json!({ "code": "not_a_member", "retriable": false })),
            )
        };

        let user = self
            .db()
            .get_user_by_email(email)
            .await
            .mcp()?
            .ok_or_else(not_a_member)?;

        self.db()
            .member_role(caller.org_id, user.id)
            .await
            .mcp()?
            .ok_or_else(not_a_member)?;

        Ok(user.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use df_core::leases::MAX_TTL_SECS;

    #[test]
    fn message_kinds_are_parsed_and_bad_ones_named() {
        assert_eq!(message_kind(None).unwrap(), MessageKind::Note);
        assert_eq!(message_kind(Some("")).unwrap(), MessageKind::Note);
        assert_eq!(message_kind(Some("request")).unwrap(), MessageKind::Request);
        assert_eq!(
            message_kind(Some("response")).unwrap(),
            MessageKind::Response
        );

        let err = message_kind(Some("urgent")).unwrap_err();
        assert!(err.message.contains("urgent"));
        assert!(
            err.message.contains("note"),
            "the error should list the valid kinds"
        );
    }

    /// A malformed lease id must be refused with something an agent can act on,
    /// not passed to the database to fail there.
    #[test]
    fn lease_ids_must_be_uuids() {
        assert!(lease_id("not-a-uuid").is_err());
        let err = lease_id("job-4").unwrap_err();
        assert!(err.message.contains("acquire_lease"));
        assert!(lease_id("  0192f0c8-0000-7000-8000-000000000000  ").is_ok());
    }

    /// The advertised ceiling has to stay inside what `df-core` will actually
    /// grant, or the tool documents a TTL the queue silently clamps away.
    #[test]
    fn the_documented_lease_ceiling_matches_the_domain() {
        assert_eq!(MAX_TTL_SECS, 4 * 3600, "the description says 4 hours");
    }
}
