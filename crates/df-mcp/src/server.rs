//! The MCP service: the shared state a tool call runs against, and the shape
//! every tool has in common.
//!
//! ## Where the caller comes from
//!
//! `rmcp` creates one service instance per MCP *session*, and a session spans
//! many HTTP requests. The authenticated principal therefore cannot live on the
//! service — it belongs to the request. The transport carries each request's
//! [`http::request::Parts`] into the tool call's context, and
//! [`crate::auth::require_bearer`] has already inserted a [`Principal`] into
//! that request's extensions, so every handler starts with
//! `Extension(parts): Extension<Parts>` and calls [`Factory::caller`].
//!
//! That indirection is doing real work: it is what makes token revocation take
//! effect on the next call rather than at the end of a session that may last
//! for days.
//!
//! ## Where the tenant comes from
//!
//! From the token, and only from the token. A token's org is fixed at issuance,
//! so no tool accepts an org argument and no tool consults one from anywhere
//! else. [`Factory::tx`] opens a transaction pinned to it, which is the only
//! way `df-core` will hand out tenant data at all.
//!
//! ## What a tool body looks like
//!
//! Check the scope, open the transaction, do exactly one thing, commit. Metering
//! (milestone 1 task 9) writes its counters inside that same transaction, before
//! the commit — a failed call must not be billed, and a successful one must not
//! be billed twice, and only a shared transaction gives both.

use std::sync::Arc;

use df_auth::tokens::Principal;
use df_billing::Meter;
use df_core::watch::Watcher;
use df_core::{Db, Tx};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::error;

/// What the MCP surface tells a client it is, in the client's own words.
///
/// Read by an LLM before it calls anything, so it is written as operating
/// guidance rather than marketing: the three things an agent gets wrong on its
/// first session are naming the repo, claiming without checking `ready`, and
/// polling instead of waiting.
const INSTRUCTIONS: &str = "\
dark-factory coordinates agentic coding work across a team. Everything here is \
anchored on repositories: a job belongs to a repo, a lease is taken on a repo's \
branch, and a tool that cannot work out which repo you mean will refuse rather \
than guess.

Getting started in a new session:
  1. Call whoami to see which organization this token opens and what you may do.
  2. Identify your repo once. Pass `remote` with the output of \
`git remote get-url origin`, or `repo` with a registered slug. If nothing \
matches, list_repos shows what is registered and register_repo adds this one.
  3. Call ready to see claimable work, claim_jobs to take it, and complete_job \
or fail_job when you are done. Never start work you have not claimed: claiming \
is what stops two agents doing the same job.

Before editing a branch, take a lease on it with acquire_lease and renew it \
while you work. Leases are advisory — the server cannot see your git \
operations — so they make collisions visible rather than impossible. \
list_leases answers 'who else is in this repo right now'.

Use watch instead of polling. It blocks until something in your organization \
changes and returns 'changed' or 'timeout'; call it again either way. Your own \
messages never wake you.

Jobs carry a free-form `metadata` object that this server never reads or \
interprets. Put whatever your own workflow needs in it.

You are billed for work performed, not for looking: reads, watch, and lease \
renewals cost nothing, while queueing, claiming, completing and messaging \
consume the plan's monthly allowance. Call usage to see where you stand.";

#[derive(Debug, Clone, Default)]
pub struct TrackerSyncConfig {
    pub github_app_id: Option<i64>,
    pub github_app_private_key: Option<String>,
    pub jira_client_id: Option<String>,
    pub jira_client_secret: Option<String>,
    pub encryption_key: Option<String>,
}

/// The service. Cheap to clone, because `rmcp` builds one per session.
#[derive(Clone)]
pub struct Factory {
    db: Db,
    watcher: Arc<Watcher>,
    meter: Arc<Meter>,
    tracker_sync: Arc<TrackerSyncConfig>,
    tool_router: ToolRouter<Self>,
}

impl Factory {
    pub fn new(db: Db, watcher: Arc<Watcher>, meter: Meter) -> Self {
        Self::new_with_tracker_sync(db, watcher, meter, TrackerSyncConfig::default())
    }

    pub fn new_with_tracker_sync(
        db: Db,
        watcher: Arc<Watcher>,
        meter: Meter,
        tracker_sync: TrackerSyncConfig,
    ) -> Self {
        Self {
            db,
            watcher,
            meter: Arc::new(meter),
            tracker_sync: Arc::new(tracker_sync),
            tool_router: crate::tools::router(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn watcher(&self) -> &Arc<Watcher> {
        &self.watcher
    }

    pub fn meter(&self) -> &Meter {
        &self.meter
    }

    pub fn tracker_sync(&self) -> &TrackerSyncConfig {
        &self.tracker_sync
    }

    /// Meter this call inside the tool's own transaction, refusing it if the
    /// org is out of budget.
    ///
    /// Called immediately after the transaction opens and **before** the tool
    /// does anything. That ordering is what makes "a failed call is not billed"
    /// true rather than aspirational: the usage row lives in the same
    /// transaction as the work, so a tool that returns an error rolls back the
    /// meter along with everything else. Charging afterwards would need a
    /// second transaction, which can fail on its own, and a bill that
    /// disagrees with what happened is worse than no bill.
    ///
    /// It also puts the quota check where a refusal is cheapest — before the
    /// work, not after it.
    ///
    /// `sync_ticket` (`tools::jobs`) is a second exception, alongside
    /// `watch`: its "work" is an outbound call to the tracker that has
    /// already happened by the time any Tx opens, so charging in the same Tx
    /// as the DB write-back would let a quota refusal roll back loop-safety
    /// state (`remote_revision`, rotated credentials) for a tracker call that
    /// cannot be un-made. It writes back first, in its own Tx, then charges
    /// in a second one — a quota refusal there means the call is not billed
    /// and the caller sees an error, but the write-back survives so a retry
    /// does not re-post to the tracker.
    pub async fn charge(
        &self,
        tx: &mut Tx<'_>,
        caller: &Principal,
        tool: &str,
    ) -> Result<df_billing::Charge, ErrorData> {
        self.meter
            .charge(tx, caller.user_id, tool)
            .await
            .map_err(|e| error::from_billing(&e))
    }

    /// The principal for the HTTP request this tool call arrived on.
    pub fn caller(&self, parts: &http::request::Parts) -> Result<Principal, ErrorData> {
        crate::auth::principal_from(parts).ok_or_else(error::unauthenticated)
    }

    /// Open a transaction pinned to the caller's org.
    pub async fn tx(&self, caller: &Principal) -> Result<Tx<'static>, ErrorData> {
        self.db
            .begin(caller.org_id)
            .await
            .map_err(|e| error::from_core(&e))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Factory {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is #[non_exhaustive], so this is built by mutation
        // rather than a struct literal with `..`.
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

/// Map a `df-core` or `df-auth` failure into the MCP envelope at the point of
/// the call.
///
/// An extension trait rather than `From` impls plus `?`, because both error
/// types and the target are foreign to this crate and the orphan rule forbids
/// the conversion. Explicit `.mcp()?` reads fine and, unlike a blanket
/// conversion, makes it visible at every call site that a domain error is
/// crossing into protocol space — which is exactly where the tenant-safety
/// review wants to look.
pub trait McpResult<T> {
    fn mcp(self) -> Result<T, ErrorData>;
}

impl<T> McpResult<T> for df_core::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_core(&e))
    }
}

impl<T> McpResult<T> for df_auth::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_auth(&e))
    }
}

impl<T> McpResult<T> for df_billing::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_billing(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instructions are the only documentation an agent gets. If they stop
    /// naming the tools an agent needs first, its opening moves get worse and
    /// nothing else in the test suite notices.
    #[test]
    fn the_instructions_name_the_opening_moves() {
        for tool in [
            "whoami",
            "list_repos",
            "register_repo",
            "ready",
            "claim_jobs",
            "complete_job",
            "acquire_lease",
            "watch",
            "usage",
        ] {
            assert!(
                INSTRUCTIONS.contains(tool),
                "the instructions should tell an agent about {tool}"
            );
        }
    }

    /// Constraint 3: coding-agent agnostic. The instructions must not name a
    /// specific client, or they become advice for one agent and noise for the
    /// rest.
    #[test]
    fn the_instructions_name_no_particular_agent() {
        let lowered = INSTRUCTIONS.to_lowercase();
        for client in ["claude", "copilot", "cursor", "codex", "gemini"] {
            assert!(
                !lowered.contains(client),
                "the instructions must stay client-neutral, but mention {client}"
            );
        }
    }
}
