//! Domain errors.
//!
//! These are written to be readable by an LLM tool caller that has never seen
//! the docs: a failure says what went wrong, what the valid options were, and
//! what to call next. `NotFound` naming the org, `UnknownRepo` listing the
//! registered slugs, and `LeaseHeld` naming the holder all exist for that
//! reason — a bare "not found" makes an agent guess, and a guessing agent
//! retries wrongly.

use crate::ids::{JobId, OrgId, UserId};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("job {0} not found")]
    JobNotFound(JobId),

    #[error("repo not found: {0}")]
    RepoNotFound(String),

    /// The agent's working directory could not be resolved to a registered repo.
    /// Deliberately never falls back to a default repo: silently queueing work
    /// against the wrong repository is worse than failing.
    #[error(
        "could not resolve a repo from {attempted}. Registered repos: {known}. \
         Register this one with register_repo, or pass an explicit repo slug."
    )]
    RepoUnresolved { attempted: String, known: String },

    #[error("repo slug {0:?} is already registered in this org")]
    RepoSlugTaken(String),

    #[error("remote {0:?} is already registered to repo {1:?}")]
    RemoteTaken(String, String),

    #[error("job {job} is {actual}, not {expected}")]
    WrongStatus {
        job: JobId,
        actual: String,
        expected: String,
    },

    #[error("job {job} was claimed by someone else")]
    AlreadyClaimed { job: JobId },

    #[error("dependency cycle: {0} would depend on itself through {1}")]
    DependencyCycle(JobId, JobId),

    #[error("{branch} of this repo is leased by {holder} until {expires_at}")]
    LeaseHeld {
        branch: String,
        holder: String,
        expires_at: chrono::DateTime<chrono::Utc>,
    },

    #[error("lease {0} is not held by you")]
    LeaseNotHeld(String),

    #[error("org {0} not found")]
    OrgNotFound(OrgId),

    #[error("no team {slug:?} in this org. Teams: {known}")]
    TeamNotFound { slug: String, known: String },

    #[error("team slug {0:?} is already taken in this org")]
    TeamSlugTaken(String),

    /// Refused rather than cascaded: a null `team_id` means org-wide, so
    /// deleting a team that still owns repos would publish them to the whole
    /// org without saying so.
    #[error(
        "this team still owns repos ({repos}). Reassign or unassign them first, \
         then delete the team — deleting it now would make them visible org-wide."
    )]
    TeamInUse { repos: String },

    #[error("user {0} is not a member of this org")]
    NotAMember(UserId),

    #[error("{email} is already a member of this org, as {role}")]
    AlreadyAMember { email: String, role: String },

    /// Unknown, already accepted, and expired collapse into one answer — which
    /// of the three it was is not something the holder of a failing token
    /// should be able to determine.
    #[error("this invitation is no longer valid. Ask an admin of the org to send a new one.")]
    InviteInvalid,

    #[error(
        "this invitation was sent to {invited}, but you are signed in as {signed_in_as}. \
         Sign in as {invited} to accept it."
    )]
    InviteWrongAccount {
        invited: String,
        signed_in_as: String,
    },

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl Error {
    /// A stable, machine-readable code for the MCP error envelope. Agents
    /// branch on this; humans read the message.
    pub fn code(&self) -> &'static str {
        match self {
            Error::JobNotFound(_) => "job_not_found",
            Error::RepoNotFound(_) => "repo_not_found",
            Error::RepoUnresolved { .. } => "repo_unresolved",
            Error::RepoSlugTaken(_) => "repo_slug_taken",
            Error::RemoteTaken(..) => "remote_taken",
            Error::WrongStatus { .. } => "wrong_status",
            Error::AlreadyClaimed { .. } => "already_claimed",
            Error::DependencyCycle(..) => "dependency_cycle",
            Error::LeaseHeld { .. } => "lease_held",
            Error::LeaseNotHeld(_) => "lease_not_held",
            Error::OrgNotFound(_) => "org_not_found",
            Error::TeamNotFound { .. } => "team_not_found",
            Error::TeamSlugTaken(_) => "team_slug_taken",
            Error::TeamInUse { .. } => "team_in_use",
            Error::NotAMember(_) => "not_a_member",
            Error::AlreadyAMember { .. } => "already_a_member",
            Error::InviteInvalid => "invite_invalid",
            Error::InviteWrongAccount { .. } => "invite_wrong_account",
            Error::Invalid(_) => "invalid_argument",
            Error::Db(_) => "internal_error",
        }
    }

    /// Whether retrying the identical call could plausibly succeed. `LeaseHeld`
    /// is retriable (the lease expires); `DependencyCycle` is not (the request
    /// is wrong). Agents use this to decide between backing off and rethinking.
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            Error::LeaseHeld { .. } | Error::AlreadyClaimed { .. } | Error::Db(_)
        )
    }
}
