//! Job tools — the queue itself.
//!
//! The lifecycle is `pending → in-progress → completed | failed`, with
//! `repend_job` returning a terminal job to `pending` for one more dispatch.
//! The one rule an agent has to internalize is that **claiming is what makes
//! work yours**: `ready` shows what is claimable, `claim_jobs` takes it
//! atomically, and starting work you have not claimed is how two agents end up
//! writing the same file.

use df_core::ids::JobId;
use df_core::jobs::{Job, JobFilter, NewJob, Status, Tracker};
use df_core::trackers::{
    decode_stored_secret, get_connection, resolve_binding, upsert_connection, Provider,
    TrackerBinding, TrackerConnection,
};
use df_trackers::github::GithubAppClient;
use df_trackers::jira::JiraClient;
use df_trackers::sync::{
    normalize_remote_revision, outbound_decision, select_jira_transition, JobTransition,
};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::{maybe_repo_of, out, repo_of, scope};
use crate::server::{Factory, McpResult};

/// Turn caller-supplied job ids into the domain type.
fn ids(raw: Vec<String>) -> Vec<JobId> {
    raw.into_iter().map(JobId::from).collect()
}

fn provider_of(tracker: Tracker) -> Provider {
    match tracker {
        Tracker::Github => Provider::Github,
        Tracker::Jira => Provider::Jira,
    }
}

/// Turn a transient tracker-sync failure (a `String` error from the shared
/// binding-resolution step or an outbound `sync_github_job`/`sync_jira_job`
/// call) into the tool's own error, distinct from the fixed, non-retriable
/// `Error::Invalid` messages `sync_ticket` returns for a configuration
/// problem it can already name precisely (`NotConfigured`/`Broken`). Every
/// site this wraps is something a later retry can plausibly fix — a
/// database hiccup resolving the binding, a tracker outage, a rate limit —
/// which is what `retriable: true` promises the caller.
fn tracker_sync_error(message: impl Into<String>, retriable: bool) -> ErrorData {
    ErrorData::new(
        rmcp::model::ErrorCode::INTERNAL_ERROR,
        message.into(),
        Some(serde_json::json!({
            "code": "tracker_sync_failed",
            "retriable": retriable,
        })),
    )
}

fn parse_github_ticket_ref(ticket_ref: &str) -> Option<(&str, &str, i64)> {
    let (owner_repo, issue) = ticket_ref.split_once('#')?;
    let (owner, repo) = owner_repo.split_once('/')?;
    let issue_number = issue.parse().ok()?;
    Some((owner, repo, issue_number))
}

struct JiraSyncOutcome {
    remote_revision: Option<String>,
    rotated_credentials: Option<df_core::crypto::Sealed>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddJobArgs {
    /// One line saying what needs doing. This is what other agents and humans
    /// see in listings, so make it specific.
    pub title: String,
    /// Everything the agent that picks this up will need. It may have none of
    /// your context.
    #[serde(default)]
    pub description: Option<String>,
    /// Registered repo slug this job belongs to.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or the git remote URL identifying it, from `git remote get-url origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Issue or ticket this job corresponds to, as a free-form reference such
    /// as "PROJ-123" or "acme/api#42". Recorded, not resolved.
    #[serde(default)]
    pub ticket_ref: Option<String>,
    /// Free-form hint about which kind of agent should take this. Never
    /// enforced — any agent may claim any job.
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Arbitrary JSON this server stores and never interprets. Your own
    /// workflow's fields go here.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Job ids that must be completed before this one can be claimed.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobArgs {
    /// The job id, like "job-42".
    pub job: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListJobsArgs {
    /// Only jobs in this state: "pending", "in-progress", "completed" or
    /// "failed". Omit for all of them.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// Only jobs you queued yourself. Defaults to false.
    #[serde(default)]
    pub mine: bool,
    /// Maximum rows. Defaults to the server's own limit.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoScopeArgs {
    /// Narrow to one repo by slug. Omit to cover the whole organization.
    #[serde(default)]
    pub repo: Option<String>,
    /// Or narrow by git remote URL.
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJobArgs {
    pub job: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Replaces the whole metadata object; it is not merged.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimJobsArgs {
    /// The job ids to take. All or none succeed.
    pub jobs: Vec<String>,
    /// How you want to be identified to teammates looking at the queue, for
    /// example "api-agent@ci-7". Free-form.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompleteJobArgs {
    pub job: String,
    /// What you did, for whoever reads this later.
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailJobArgs {
    pub job: String,
    /// Why it failed, specifically enough that the next attempt can do better.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetDependenciesArgs {
    /// The job whose dependencies are changing.
    pub job: String,
    /// Job ids this job should wait for.
    #[serde(default)]
    pub add: Vec<String>,
    /// Job ids it should stop waiting for.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkTicketArgs {
    /// The job to attach a tracker ticket to.
    pub job: String,
    /// Which tracker the ticket lives in: "jira" or "github".
    pub tracker: Tracker,
    /// The ticket's reference in that tracker, e.g. "PROJ-123" or
    /// "acme/api#42". Recorded, not resolved.
    pub ticket_ref: String,
}

/// The outcome of looking up whether a job's repo actually has somewhere to
/// sync to. Kept three-way rather than collapsing to `Option` so a broken
/// invariant (a binding whose `connection_id` points at no
/// `tracker_connections` row) never gets reported the same way as an
/// ordinary "nothing configured yet" gap — the two need different messages
/// wherever a caller can see them (`sync_ticket`), and the same distinction
/// is worth preserving even where nothing is currently surfaced
/// (`sync_job_after_transition`, which maps both to a silent no-op).
enum BindingLookup {
    /// No `tracker_bindings` row for this repo/provider, or one exists but
    /// `connection_id IS NULL` (configured, not yet activated). Both mean the
    /// same thing to a caller: nothing is wired up yet.
    NotConfigured,
    /// The binding's `connection_id` names a connection that no longer
    /// exists — a data-integrity problem, not a configuration gap. Already
    /// logged as the invariant violation it is by the time this is returned.
    Broken,
    Ready(Box<TrackerBinding>, TrackerConnection),
}

#[tool_router(router = jobs_router, vis = "pub(crate)")]
impl Factory {
    /// Resolve the connection a job's repo should sync outbound writes
    /// through, in its own short transaction (no HTTP call is ever made while
    /// holding one open). Shared by the fire-and-forget post-transition sync
    /// and `sync_ticket`, which react to the three-way result differently —
    /// see [`BindingLookup`].
    async fn resolve_tracker_binding(
        &self,
        org_id: df_core::ids::OrgId,
        repo_id: df_core::ids::RepoId,
        provider: Provider,
    ) -> Result<BindingLookup, String> {
        let mut tx = self
            .db()
            .begin(org_id)
            .await
            .map_err(|error| error.to_string())?;
        let binding = resolve_binding(&mut tx, repo_id, provider)
            .await
            .map_err(|error| error.to_string())?;
        let Some(binding) = binding else {
            tx.commit().await.map_err(|error| error.to_string())?;
            return Ok(BindingLookup::NotConfigured);
        };
        let Some(connection_id) = binding.connection_id else {
            tx.commit().await.map_err(|error| error.to_string())?;
            return Ok(BindingLookup::NotConfigured);
        };
        let connection = get_connection(&mut tx, provider)
            .await
            .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        // `get_connection` looks up by (org, provider), not by id.
        // `tracker_bindings.connection_id` has `ON DELETE SET NULL` against
        // `tracker_connections`, and `tracker_connections` carries
        // `UNIQUE (org_id, provider)` — together those mean a non-null
        // `connection_id` is, today, always the id of the one connection row
        // `get_connection` would return for this provider; there is no
        // current write path that leaves it pointing at a since-replaced
        // row. Comparing ids explicitly costs nothing and keeps that true
        // even if a future change (a differently-behaved delete path, a
        // relaxed constraint) would otherwise let this resolve silently
        // through the wrong connection instead of surfacing `Broken`.
        let connection = match connection {
            Some(connection) if connection.id == connection_id => connection,
            _ => {
                // Reachable today only if the invariant above stops holding.
                // Log it as the invariant violation it would be instead of
                // letting outbound sync silently vanish with no operator
                // signal, matching how the inbound webhook route treats the
                // same broken-invariant shape.
                tracing::error!(
                    repo_id = %repo_id,
                    provider = %provider,
                    "tracker binding names a connection id that no longer matches this org's \
                     tracker_connections row for this provider"
                );
                return Ok(BindingLookup::Broken);
            }
        };
        Ok(BindingLookup::Ready(Box::new(binding), connection))
    }

    async fn sync_jobs_after_transition(
        &self,
        jobs: &[Job],
        transition: JobTransition,
        detail: Option<&str>,
    ) {
        for job in jobs {
            if let Err(error) = self
                .sync_job_after_transition(job, transition, detail)
                .await
            {
                tracing::error!(job_id = %job.id, error = %error, "tracker write-back failed");
            }
        }
    }

    async fn sync_job_after_transition(
        &self,
        job: &Job,
        transition: JobTransition,
        detail: Option<&str>,
    ) -> Result<(), String> {
        let Some(tracker) = job.tracker else {
            return Ok(());
        };
        let Some(ticket_ref) = job.ticket_ref.as_deref() else {
            return Ok(());
        };

        let provider = provider_of(tracker);
        let (binding, connection) = match self
            .resolve_tracker_binding(job.org_id, job.repo_id, provider)
            .await?
        {
            // Nothing to sync to yet, or a broken binding an operator needs
            // to fix — neither is a failure of the queue transition that
            // just happened, so both are a silent no-op here exactly as
            // before this helper was extracted.
            BindingLookup::NotConfigured | BindingLookup::Broken => return Ok(()),
            BindingLookup::Ready(binding, connection) => (binding, connection),
        };

        let plan = outbound_decision(transition, tracker, detail);
        match tracker {
            Tracker::Github => {
                let outcome = self
                    .sync_github_job(job, ticket_ref, &connection.external_id, &plan)
                    .await?;
                if let Some(remote_revision) = outcome {
                    let mut tx = self
                        .db()
                        .begin(job.org_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    tx.set_remote_revision(&job.id, &remote_revision)
                        .await
                        .map_err(|error| error.to_string())?;
                    tx.commit().await.map_err(|error| error.to_string())?;
                }
            }
            Tracker::Jira => {
                let outcome = self
                    .sync_jira_job(job, ticket_ref, &binding.external_ref, &connection, &plan)
                    .await?;
                if outcome.remote_revision.is_some() || outcome.rotated_credentials.is_some() {
                    let mut tx = self
                        .db()
                        .begin(job.org_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(remote_revision) = outcome.remote_revision.as_deref() {
                        tx.set_remote_revision(&job.id, remote_revision)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    if let Some(rotated) = outcome.rotated_credentials.as_ref() {
                        let webhook_secret = connection
                            .encrypted_webhook_secret
                            .as_deref()
                            .map(decode_stored_secret)
                            .transpose()
                            .map_err(|error| error.to_string())?;
                        upsert_connection(
                            &mut tx,
                            Provider::Jira,
                            &connection.external_id,
                            Some(rotated),
                            webhook_secret.as_ref(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                    tx.commit().await.map_err(|error| error.to_string())?;
                }
            }
        }

        Ok(())
    }

    async fn sync_github_job(
        &self,
        job: &Job,
        ticket_ref: &str,
        external_id: &str,
        plan: &df_trackers::sync::OutboundDecision,
    ) -> Result<Option<String>, String> {
        let app_id = self
            .tracker_sync()
            .github_app_id
            .ok_or_else(|| "GitHub tracker sync is not configured (missing App id)".to_string())?;
        let private_key = self
            .tracker_sync()
            .github_app_private_key
            .clone()
            .ok_or_else(|| {
                "GitHub tracker sync is not configured (missing App private key)".to_string()
            })?;
        let installation_id = external_id.parse::<i64>().map_err(|error| {
            format!(
                "tracker connection for job {} has a non-numeric GitHub installation id {external_id:?}: {error}",
                job.id
            )
        })?;
        let (owner, repo, issue_number) = parse_github_ticket_ref(ticket_ref).ok_or_else(|| {
            format!(
                "job {} has an invalid GitHub ticket_ref {ticket_ref:?}",
                job.id
            )
        })?;
        let client =
            GithubAppClient::new(app_id, private_key).map_err(|error| error.to_string())?;

        client
            .post_comment(installation_id, owner, repo, issue_number, &plan.comment)
            .await
            .map_err(|error| error.to_string())?;
        let remote_revision = if let Some(close) = plan.github_close.as_ref() {
            match client
                .set_issue_state(
                    installation_id,
                    owner,
                    repo,
                    issue_number,
                    "closed",
                    Some(&close.state_reason),
                )
                .await
            {
                Ok(updated_at) => updated_at,
                Err(error) => return Err(error.to_string()),
            }
        } else {
            client
                .get_issue_updated_at(installation_id, owner, repo, issue_number)
                .await
                .map_err(|error| error.to_string())?
        };

        Ok(normalize_remote_revision(remote_revision.as_deref()))
    }

    async fn sync_jira_job(
        &self,
        job: &Job,
        ticket_ref: &str,
        binding_external_ref: &str,
        connection: &df_core::trackers::TrackerConnection,
        plan: &df_trackers::sync::OutboundDecision,
    ) -> Result<JiraSyncOutcome, String> {
        let client_id =
            self.tracker_sync().jira_client_id.clone().ok_or_else(|| {
                "JIRA tracker sync is not configured (missing client id)".to_string()
            })?;
        let client_secret = self
            .tracker_sync()
            .jira_client_secret
            .clone()
            .ok_or_else(|| {
                "JIRA tracker sync is not configured (missing client secret)".to_string()
            })?;
        let encryption_key = self
            .tracker_sync()
            .encryption_key
            .as_deref()
            .ok_or_else(|| {
                "JIRA tracker sync is not configured (missing encryption key)".to_string()
            })?;
        let cipher = df_core::crypto::Cipher::from_base64_key(encryption_key)
            .map_err(|error| error.to_string())?;
        let encoded = connection.encrypted_credentials.as_deref().ok_or_else(|| {
            format!(
                "JIRA connection for job {} is missing stored credentials",
                job.id
            )
        })?;
        let sealed = decode_stored_secret(encoded).map_err(|error| error.to_string())?;
        let refresh_token =
            JiraClient::open_refresh_token(&cipher, &sealed).map_err(|error| error.to_string())?;
        let client =
            JiraClient::new(client_id, client_secret).map_err(|error| error.to_string())?;
        let tokens = client
            .refresh_access_token(&refresh_token)
            .await
            .map_err(|error| error.to_string())?;

        client
            .post_comment(
                &tokens.access_token,
                &connection.external_id,
                ticket_ref,
                &plan.comment,
            )
            .await
            .map_err(|error| error.to_string())?;

        if let Some(target) = plan.jira_transition.as_ref() {
            let transitions = client
                .list_transitions(&tokens.access_token, &connection.external_id, ticket_ref)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(transition) = select_jira_transition(target, &transitions) {
                client
                    .transition_issue(
                        &tokens.access_token,
                        &connection.external_id,
                        ticket_ref,
                        &transition.id,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            } else {
                tracing::warn!(
                    job_id = %job.id,
                    provider = "jira",
                    external_ref = %binding_external_ref,
                    ticket_ref = %ticket_ref,
                    "no reachable JIRA transition matched the requested target; comment posted only"
                );
            }
        }

        let remote_revision = client
            .get_issue_updated_at(&tokens.access_token, &connection.external_id, ticket_ref)
            .await
            .map_err(|error| error.to_string())?;
        let rotated_credentials = if tokens.refresh_token != refresh_token {
            Some(
                tokens
                    .seal_refresh_token(&cipher)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };

        Ok(JiraSyncOutcome {
            remote_revision: normalize_remote_revision(remote_revision.as_deref()),
            rotated_credentials,
        })
    }

    #[tool(
        name = "add_job",
        description = "Queue a new job against a repository. Give it a specific title and \
                       enough description that an agent with none of your context can pick it \
                       up. Use dependsOn for work that must wait for other jobs. Returns the \
                       created job, including its id."
    )]
    pub async fn add_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<AddJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "add_job").await?;
        let repo = repo_of(&mut tx, args.repo, args.remote).await?;
        let job = tx
            .add_job(NewJob {
                repo_id: repo.id,
                title: args.title,
                description: args.description,
                ticket_ref: args.ticket_ref,
                agent_type: args.agent_type,
                metadata: args.metadata,
                depends_on: ids(args.depends_on),
                created_by: Some(caller.user_id),
                ..Default::default()
            })
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "get_job",
        description = "Fetch one job by id, with its full description, metadata, status and who \
                       claimed it."
    )]
    pub async fn get_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "get_job").await?;
        let job = tx.get_job(&JobId::from(args.job)).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "list_jobs",
        description = "List jobs, optionally filtered by status, repository, or whether you \
                       queued them. To find work you can actually take, prefer `ready` — this \
                       returns jobs regardless of whether their dependencies are satisfied."
    )]
    pub async fn list_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        // Parse before opening a transaction: a bad status is the caller's
        // mistake and does not need a database round trip to detect.
        let status = args
            .status
            .as_deref()
            .map(str::parse::<Status>)
            .transpose()
            .mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_jobs").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx
            .list_jobs(&JobFilter {
                status,
                repo_id,
                created_by: args.mine.then_some(caller.user_id),
                limit: args.limit,
                ..Default::default()
            })
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "update_job",
        description = "Change a job's title, description, agent type hint, or metadata. Only \
                       the fields you pass change, except metadata, which is replaced whole. \
                       Does not change status — use claim_jobs, complete_job, fail_job or \
                       repend_job for that."
    )]
    pub async fn update_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_job").await?;
        let job = tx
            .update_job(
                &JobId::from(args.job),
                args.title.as_deref(),
                args.description.as_deref(),
                args.agent_type.as_deref(),
                args.metadata.as_ref(),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "delete_job",
        description = "Remove a job from the queue permanently. Prefer fail_job for work that \
                       was attempted and did not succeed: deleting loses the history of why."
    )]
    pub async fn delete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::DeletedOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let id = JobId::from(args.job);
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "delete_job").await?;
        tx.delete_job(&id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DeletedOut { deleted: id }))
    }

    #[tool(
        name = "claim_jobs",
        description = "Atomically take one or more pending jobs, moving them to in-progress \
                       under your name. All of them succeed or none do, so a partial claim can \
                       never leave you believing you own work you do not. Fails if any job is \
                       already claimed or still blocked by an unfinished dependency. Claim \
                       before you start working."
    )]
    pub async fn claim_jobs(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ClaimJobsArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "claim_jobs").await?;
        let jobs = tx
            .claim_jobs(&ids(args.jobs), caller.user_id, args.agent.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobsOut { jobs: jobs.clone() });
        self.sync_jobs_after_transition(&jobs, JobTransition::Claimed, args.agent.as_deref())
            .await;

        Ok(out)
    }

    #[tool(
        name = "complete_job",
        description = "Mark a job you claimed as completed, with a summary of what was done. \
                       Anything that depends on it becomes claimable."
    )]
    pub async fn complete_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CompleteJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "complete_job").await?;
        let job = tx
            .complete_job(&JobId::from(args.job), args.result.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobOut { job: job.clone() });
        self.sync_jobs_after_transition(
            std::slice::from_ref(&job),
            JobTransition::Completed,
            args.result.as_deref(),
        )
        .await;

        Ok(out)
    }

    #[tool(
        name = "fail_job",
        description = "Mark a job you claimed as failed, recording why. Use this rather than \
                       leaving a job in-progress when you cannot finish it — an abandoned claim \
                       blocks everything downstream and tells nobody anything."
    )]
    pub async fn fail_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FailJobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "fail_job").await?;
        let job = tx
            .fail_job(&JobId::from(args.job), args.error.as_deref())
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        let out = Json(out::JobOut { job: job.clone() });
        self.sync_jobs_after_transition(
            std::slice::from_ref(&job),
            JobTransition::Failed,
            args.error.as_deref(),
        )
        .await;

        Ok(out)
    }

    #[tool(
        name = "repend_job",
        description = "Return a completed or failed job to pending so it can be claimed again. \
                       The attempt count is preserved, so repeated failures stay visible."
    )]
    pub async fn repend_job(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "repend_job").await?;
        let job = tx.repend_job(&JobId::from(args.job)).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "set_dependencies",
        description = "Add or remove the jobs one job waits for. A job with unfinished \
                       dependencies cannot be claimed. A change that would make a job depend \
                       on itself, directly or through a chain, is rejected. Returns the \
                       resulting dependency list."
    )]
    pub async fn set_dependencies(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetDependenciesArgs>,
    ) -> Result<Json<out::DependenciesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_WRITE).mcp()?;

        let job = JobId::from(args.job);
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "set_dependencies").await?;
        let deps = tx
            .set_dependencies(&job, &ids(args.add), &ids(args.remove))
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::DependenciesOut {
            job,
            dependencies: deps,
        }))
    }

    #[tool(
        name = "ready",
        description = "List the jobs that can be claimed right now: pending, with every \
                       dependency completed. This is the tool to call when looking for work."
    )]
    pub async fn ready(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "ready").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx.ready(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "blocked",
        description = "List pending jobs that cannot be claimed yet because something they \
                       depend on is unfinished. Useful for working out what to unblock first."
    )]
    pub async fn blocked(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::JobsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "blocked").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let jobs = tx.blocked(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobsOut { jobs }))
    }

    #[tool(
        name = "stats",
        description = "Counts of jobs by state — pending, in-progress, completed, failed, and \
                       how many of the pending ones are blocked — for the whole organization or \
                       one repository."
    )]
    pub async fn stats(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RepoScopeArgs>,
    ) -> Result<Json<out::StatsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::JOBS_READ).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "stats").await?;
        let repo_id = maybe_repo_of(&mut tx, args.repo, args.remote).await?;
        let stats = tx.stats(repo_id).await.mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::StatsOut { stats }))
    }

    #[tool(
        name = "link_ticket",
        description = "Attach a tracker ticket to a job so future transitions (claim, complete, \
                       fail) post updates to it, and so sync_ticket can force a write-back on \
                       demand. Use this for a job that was queued by hand (add_job) or claimed \
                       before anyone thought to link it — a job created from an inbound webhook \
                       already has this set. Fails with ticket_already_linked if another live \
                       job in this repo already owns that ticket_ref."
    )]
    pub async fn link_ticket(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<LinkTicketArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::TRACKERS).mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "link_ticket").await?;
        let job = tx
            .link_ticket(&JobId::from(args.job), args.tracker, &args.ticket_ref)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;

        Ok(Json(out::JobOut { job }))
    }

    #[tool(
        name = "sync_ticket",
        description = "Force an immediate outbound write-back to the ticket a job is linked to, \
                       reflecting the job's current status (in-progress, completed, or failed) \
                       as a comment and, where the tracker supports it, a status transition. \
                       Use this after link_ticket, when nothing has been posted yet because no \
                       transition has fired since the link was made, or to retry after a \
                       tracker outage — unlike the automatic write-back after claim_jobs, \
                       complete_job and fail_job, this call surfaces a tracker failure as its \
                       own error rather than swallowing it, because talking to the tracker is \
                       the entire point of calling it. Requires the job to already be linked \
                       via link_ticket and to be in-progress, completed, or failed."
    )]
    pub async fn sync_ticket(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<JobArgs>,
    ) -> Result<Json<out::JobOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(scope::TRACKERS).mcp()?;

        // Reading the job costs nothing to bill for: the outbound call is the
        // entire point of this tool, and whether it succeeds is not known
        // until after it returns. Charging here, before that call, would bill
        // a request whose tracker round trip then fails — charging happens
        // later, only on success, alongside the write-back (see below).
        let mut tx = self.tx(&caller).await?;
        let job = tx.get_job(&JobId::from(args.job)).await.mcp()?;
        tx.commit().await.mcp()?;

        let Some(tracker) = job.tracker else {
            return Err(df_core::Error::Invalid(format!(
                "job {} has no tracker linked — call link_ticket first",
                job.id
            )))
            .mcp();
        };
        let Some(ticket_ref) = job.ticket_ref.clone() else {
            return Err(df_core::Error::Invalid(format!(
                "job {} has no ticket_ref linked — call link_ticket first",
                job.id
            )))
            .mcp();
        };
        let (transition, detail) = match job.status {
            Status::Pending => {
                return Err(df_core::Error::Invalid(format!(
                    "job {} is still pending — nothing has happened to it yet to sync to its ticket",
                    job.id
                )))
                .mcp();
            }
            Status::InProgress => (JobTransition::Claimed, job.claimed_by_label.clone()),
            Status::Completed => (JobTransition::Completed, job.result.clone()),
            Status::Failed => (JobTransition::Failed, job.error.clone()),
        };

        let provider = provider_of(tracker);
        let (binding, connection) = match self
            .resolve_tracker_binding(job.org_id, job.repo_id, provider)
            .await
            .map_err(|error| tracker_sync_error(error, true))?
        {
            BindingLookup::NotConfigured => {
                return Err(df_core::Error::Invalid(format!(
                    "this repo has no active {provider} binding — configure one before \
                     calling sync_ticket"
                )))
                .mcp();
            }
            BindingLookup::Broken => {
                return Err(df_core::Error::Invalid(format!(
                    "this repo's {provider} binding points at a connection that no longer \
                     exists — this needs an operator to fix, not a retry"
                )))
                .mcp();
            }
            BindingLookup::Ready(binding, connection) => (binding, connection),
        };

        let plan = outbound_decision(transition, tracker, detail.as_deref());
        let job = match tracker {
            Tracker::Github => {
                // A malformed ticket_ref (anything not "owner/repo#N") is
                // caller input, not a tracker outage: it will never succeed
                // no matter how many times it's retried. Catching it here,
                // ahead of the outbound call, keeps it out of
                // `tracker_sync_error`'s retriable bucket — the parse
                // failure inside `sync_github_job` itself is a `String`
                // with no room to carry that distinction, so it must not be
                // the thing this checks.
                if parse_github_ticket_ref(&ticket_ref).is_none() {
                    return Err(df_core::Error::Invalid(format!(
                        "job {} has a ticket_ref {ticket_ref:?} that is not a valid GitHub \
                         issue reference (expected \"owner/repo#number\") — relink it with \
                         link_ticket before calling sync_ticket again",
                        job.id
                    )))
                    .mcp();
                }
                let outcome = self
                    .sync_github_job(&job, &ticket_ref, &connection.external_id, &plan)
                    .await
                    .map_err(|error| tracker_sync_error(error, true))?;
                // The write-back commits on its own, before charging is even
                // attempted: this is loop-safety state for a call to the
                // tracker that has already happened, and it must survive
                // regardless of whether billing this call succeeds. Charging
                // in the same Tx as the write-back would roll the write-back
                // back on a quota refusal, leaving remote_revision stale and
                // making a caller's retry re-post to the tracker a second
                // time for a sync that already went through.
                let mut tx = self.tx(&caller).await?;
                let job = if let Some(remote_revision) = outcome {
                    tx.set_remote_revision(&job.id, &remote_revision)
                        .await
                        .mcp()?;
                    let job = tx.get_job(&job.id).await.mcp()?;
                    tx.commit().await.mcp()?;
                    job
                } else {
                    tx.commit().await.mcp()?;
                    job
                };
                let mut charge_tx = self.tx(&caller).await?;
                self.charge(&mut charge_tx, &caller, "sync_ticket").await?;
                charge_tx.commit().await.mcp()?;
                job
            }
            Tracker::Jira => {
                // Same reasoning as the GitHub arm's pre-parse above: an
                // issue key that doesn't match JIRA's own `PROJECT-123`
                // grammar is caller input, not a tracker outage, and will
                // never succeed no matter how many times it's retried.
                // `sync_jira_job` also validates this (`Client::*` calls
                // `validate_jira_issue_key` before building a request), but
                // that failure comes back as a `String` with no room to
                // mark it non-retriable — checking it here, ahead of the
                // call, is what keeps it out of `tracker_sync_error`'s
                // retriable bucket.
                if df_trackers::jira::validate_jira_issue_key(&ticket_ref).is_err() {
                    return Err(df_core::Error::Invalid(format!(
                        "job {} has a ticket_ref {ticket_ref:?} that is not a valid JIRA \
                         issue key (expected \"PROJECT-123\") — relink it with link_ticket \
                         before calling sync_ticket again",
                        job.id
                    )))
                    .mcp();
                }
                let outcome = self
                    .sync_jira_job(&job, &ticket_ref, &binding.external_ref, &connection, &plan)
                    .await
                    .map_err(|error| tracker_sync_error(error, true))?;
                // Same reasoning as the GitHub arm above: the write-back (the
                // revision and any rotated JIRA credentials) commits before
                // charging is attempted, so a quota refusal never erases
                // loop-safety state for a tracker call that already happened.
                let mut tx = self.tx(&caller).await?;
                let job = if let Some(remote_revision) = outcome.remote_revision.as_deref() {
                    tx.set_remote_revision(&job.id, remote_revision)
                        .await
                        .mcp()?;
                    tx.get_job(&job.id).await.mcp()?
                } else {
                    job
                };
                if let Some(rotated) = outcome.rotated_credentials.as_ref() {
                    let webhook_secret = connection
                        .encrypted_webhook_secret
                        .as_deref()
                        .map(decode_stored_secret)
                        .transpose()
                        .mcp()?;
                    upsert_connection(
                        &mut tx,
                        Provider::Jira,
                        &connection.external_id,
                        Some(rotated),
                        webhook_secret.as_ref(),
                    )
                    .await
                    .mcp()?;
                }
                tx.commit().await.mcp()?;
                let mut charge_tx = self.tx(&caller).await?;
                self.charge(&mut charge_tx, &caller, "sync_ticket").await?;
                charge_tx.commit().await.mcp()?;
                job
            }
        };

        Ok(Json(out::JobOut { job }))
    }
}
