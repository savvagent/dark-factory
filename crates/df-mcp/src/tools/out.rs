//! What tools return.
//!
//! Every result is an **object with one named field**, never a bare array or a
//! bare scalar. Two reasons, and the first is not negotiable: MCP requires a
//! tool's `outputSchema` to have root type `object`, so `Json<Vec<Job>>` is not
//! a legal result. The second is that an envelope can grow — the day `list_jobs`
//! wants to return a cursor alongside the rows, `{"jobs": [...]}` gains a field
//! and every existing caller still works, where a top-level array would have to
//! break them all.
//!
//! The payloads are `df-core`'s own domain types rather than view structs
//! mirrored into this crate. A parallel set of types would need updating in two
//! places whenever a column is added, and the copy that got missed would be the
//! one agents read. `df-core` derives `JsonSchema` alongside the `Serialize` it
//! already had, which is the same concern rather than a new one.

use df_core::ids::{JobId, OrgId, UserId};
use df_core::jobs::{Job, Stats};
use df_core::leases::Lease;
use df_core::messages::Message;
use df_core::orgs::{Plan, Role};
use df_core::repos::Repo;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoOut {
    pub repo: Repo,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReposOut {
    pub repos: Vec<Repo>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobOut {
    pub job: Job,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobsOut {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeletedOut {
    pub deleted: JobId,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DependenciesOut {
    pub job: JobId,
    /// Everything this job now waits for, after the change.
    pub dependencies: Vec<JobId>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatsOut {
    pub stats: Stats,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaseOut {
    pub lease: Lease,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeasesOut {
    pub leases: Vec<Lease>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleasedOut {
    pub released: uuid::Uuid,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageOut {
    pub message: Message,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessagesOut {
    pub messages: Vec<Message>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CursorOut {
    /// Where the read cursor actually landed, which is not always what was
    /// asked for — it is clamped to the newest message that exists.
    pub cursor: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnreadOut {
    pub unread: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WatchOutcome {
    /// Something changed. Refetch whatever you care about.
    Changed,
    /// Nothing happened in the time allowed. Call `watch` again.
    Timeout,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchOut {
    pub outcome: WatchOutcome,
    /// How long the call actually blocked — not the timeout it asked for. On a
    /// wake-up these differ, and an agent pacing itself off this field needs the
    /// real number.
    pub waited_seconds: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub user: UserOut,
    pub org: OrgOut,
    /// The caller's role in this org, if they are still a member.
    pub role: Option<Role>,
    pub token: TokenOut,
    /// Where this org stands against its monthly allowance. Here as well as in
    /// `usage` because an agent that calls one thing at the start of a session
    /// calls this one, and finding out it is nearly out of budget after the
    /// work is queued is finding out too late.
    pub usage: df_billing::Status,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserOut {
    pub id: UserId,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgOut {
    pub id: OrgId,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub plan: Option<Plan>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenOut {
    /// "oauth" for a browser-authorized token, "pat" for a personal access
    /// token pasted into a client's configuration.
    pub kind: &'static str,
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageOut {
    pub usage: df_billing::Status,
}
