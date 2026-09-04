use chrono::{DateTime, SecondsFormat, Utc};
use df_core::jobs::{Job, Status, Tracker};
use df_core::trackers::{Provider, TrackerBinding};

use crate::jira::Transition;
use crate::webhook::WebhookEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDecision {
    Drop,
    Create {
        title: String,
        description: Option<String>,
        ticket_ref: String,
        remote_revision: Option<String>,
    },
    Update {
        title: String,
        description: Option<String>,
        remote_revision: Option<String>,
    },
    Close {
        to: Status,
        result: Option<String>,
        error: Option<String>,
        remote_revision: Option<String>,
    },
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTransition {
    Claimed,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubClose {
    pub state_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JiraTransitionTarget {
    NamedStatus(String),
    StatusCategory(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundDecision {
    pub comment: String,
    pub github_close: Option<GithubClose>,
    pub jira_transition: Option<JiraTransitionTarget>,
}

pub fn inbound_decision(
    event: &WebhookEvent,
    binding: &TrackerBinding,
    existing: Option<&Job>,
) -> InboundDecision {
    if !has_trigger_label(event, &binding.trigger_label) {
        return InboundDecision::Drop;
    }

    if let Some(job) = existing {
        if is_stale_or_echoed(
            job.remote_revision.as_deref(),
            event.issue.updated_at.as_deref(),
        ) {
            return InboundDecision::Drop;
        }
        if job.status.is_terminal() {
            return InboundDecision::NoOp;
        }
    }

    let ticket_ref = ticket_ref(event);
    let remote_revision = normalize_remote_revision(event.issue.updated_at.as_deref());
    let close = close_status(event);

    match existing {
        Some(_job) if close.is_some() => {
            let to = close.expect("checked above");
            InboundDecision::Close {
                to,
                result: None,
                error: None,
                remote_revision,
            }
        }
        Some(_job) => {
            if event.provider == Provider::Github
                && !matches!(
                    event.action.as_str(),
                    "opened" | "edited" | "labeled" | "closed"
                )
            {
                return InboundDecision::NoOp;
            }
            InboundDecision::Update {
                title: event.issue.title.clone(),
                description: event.issue.body.clone(),
                remote_revision,
            }
        }
        None if close.is_some() => InboundDecision::NoOp,
        None => {
            if event.provider == Provider::Github
                && !matches!(event.action.as_str(), "opened" | "edited" | "labeled")
            {
                return InboundDecision::NoOp;
            }
            InboundDecision::Create {
                title: event.issue.title.clone(),
                description: event.issue.body.clone(),
                ticket_ref,
                remote_revision,
            }
        }
    }
}

pub fn outbound_decision(
    transition: JobTransition,
    tracker: Tracker,
    detail: Option<&str>,
) -> OutboundDecision {
    let comment = match transition {
        JobTransition::Claimed => detail
            .map(|agent| format!("Claimed by {agent}."))
            .unwrap_or_else(|| "Claimed.".into()),
        JobTransition::Completed => detail
            .map(str::to_string)
            .unwrap_or_else(|| "Completed.".into()),
        JobTransition::Failed => detail
            .map(str::to_string)
            .unwrap_or_else(|| "Failed.".into()),
    };

    let github_close = matches!(
        (transition, tracker),
        (JobTransition::Completed, Tracker::Github)
    )
    .then(|| GithubClose {
        state_reason: "completed".into(),
    });

    let jira_transition = match (transition, tracker) {
        (JobTransition::Claimed, Tracker::Jira) => {
            Some(JiraTransitionTarget::NamedStatus("In Progress".into()))
        }
        (JobTransition::Completed, Tracker::Jira) => {
            Some(JiraTransitionTarget::StatusCategory("done".into()))
        }
        (JobTransition::Failed, Tracker::Jira) => {
            Some(JiraTransitionTarget::StatusCategory("new".into()))
        }
        _ => None,
    };

    OutboundDecision {
        comment,
        github_close,
        jira_transition,
    }
}

pub fn select_jira_transition<'a>(
    target: &JiraTransitionTarget,
    transitions: &'a [Transition],
) -> Option<&'a Transition> {
    transitions.iter().find(|transition| match target {
        JiraTransitionTarget::NamedStatus(name) => transition.to_status.eq_ignore_ascii_case(name),
        JiraTransitionTarget::StatusCategory(category) => {
            normalized_jira_category(&transition.to_status_category)
                .eq_ignore_ascii_case(&normalized_jira_category(category))
        }
    })
}

pub fn normalize_remote_revision(raw: Option<&str>) -> Option<String> {
    raw.and_then(parse_remote_revision)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn parse_remote_revision(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn is_stale_or_echoed(stored: Option<&str>, incoming: Option<&str>) -> bool {
    match (
        stored.and_then(parse_remote_revision),
        incoming.and_then(parse_remote_revision),
    ) {
        (Some(stored), Some(incoming)) => incoming <= stored,
        _ => false,
    }
}

fn has_trigger_label(event: &WebhookEvent, trigger_label: &str) -> bool {
    event
        .issue
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(trigger_label))
}

/// The `ticket_ref` a webhook event resolves to, in the same
/// `"{repo}#{number}"` / `"PROJ-123"` convention `add_job` documents. Exported
/// so callers doing their own job lookup before `inbound_decision` runs (the
/// webhook route needs the existing job to decide create-vs-update) derive
/// the identical string instead of risking silent drift between two
/// hand-written formats.
pub fn ticket_ref(event: &WebhookEvent) -> String {
    match event.provider {
        Provider::Github => format!("{}#{}", event.binding_external_ref, event.issue.reference),
        Provider::Jira => event.issue.reference.clone(),
    }
}

fn close_status(event: &WebhookEvent) -> Option<Status> {
    match event.provider {
        Provider::Github if event.action == "closed" => {
            if event.issue.state_reason.as_deref() == Some("not_planned") {
                Some(Status::Failed)
            } else {
                Some(Status::Completed)
            }
        }
        Provider::Jira => match normalized_jira_status(&event.issue.state).as_str() {
            "done" | "closed" | "resolved" => Some(Status::Completed),
            "won't do" | "wont do" | "cancelled" | "rejected" | "declined" => Some(Status::Failed),
            _ => None,
        },
        _ => None,
    }
}

fn normalized_jira_status(status: &str) -> String {
    status.trim().to_ascii_lowercase()
}

fn normalized_jira_category(category: &str) -> String {
    match category.trim().to_ascii_lowercase().as_str() {
        "new" | "to do" | "todo" => "new".into(),
        "done" => "done".into(),
        other => other.into(),
    }
}
