use df_core::ids::{JobId, OrgId, RepoId};
use df_core::jobs::{Job, Status, Tracker};
use df_core::trackers::{Provider, TrackerBinding};

fn binding(provider: Provider, trigger_label: &str) -> TrackerBinding {
    TrackerBinding {
        id: uuid::Uuid::new_v4(),
        org_id: OrgId::new(),
        repo_id: RepoId::new(),
        connection_id: Some(uuid::Uuid::new_v4()),
        provider,
        external_ref: match provider {
            Provider::Github => "acme/api".into(),
            Provider::Jira => "ENG".into(),
        },
        trigger_label: trigger_label.into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn job(status: Status, tracker: Tracker, ticket_ref: &str, remote_revision: Option<&str>) -> Job {
    Job {
        id: JobId::from("job-7"),
        org_id: OrgId::new(),
        repo_id: RepoId::new(),
        team_id: None,
        title: "Old title".into(),
        description: Some("Old body".into()),
        status,
        ticket_ref: Some(ticket_ref.into()),
        tracker: Some(tracker),
        remote_revision: remote_revision.map(str::to_string),
        agent_type: None,
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        started_at: None,
        completed_at: None,
        attempts: 0,
        result: None,
        error: None,
        created_by: None,
        claimed_by: None,
        claimed_by_label: None,
    }
}

fn github_issue_event(
    action: &str,
    labels: &[&str],
    updated_at: Option<&str>,
    state_reason: Option<&str>,
) -> df_trackers::webhook::WebhookEvent {
    df_trackers::webhook::WebhookEvent {
        provider: Provider::Github,
        connection_external_id: "123456".into(),
        binding_external_ref: "acme/api".into(),
        action: action.into(),
        kind: df_trackers::webhook::WebhookEventKind::Issue,
        issue: df_trackers::webhook::IssueSnapshot {
            id: "7001".into(),
            reference: "17".into(),
            title: "New title".into(),
            body: Some("New body".into()),
            state: if action == "closed" { "closed" } else { "open" }.into(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            updated_at: updated_at.map(str::to_string),
            state_reason: state_reason.map(str::to_string),
            comment: None,
        },
    }
}

fn jira_issue_event(
    state: &str,
    labels: &[&str],
    updated_at: Option<&str>,
) -> df_trackers::webhook::WebhookEvent {
    df_trackers::webhook::WebhookEvent {
        provider: Provider::Jira,
        connection_external_id: "cloud-1".into(),
        binding_external_ref: "ENG".into(),
        action: "updated".into(),
        kind: df_trackers::webhook::WebhookEventKind::Issue,
        issue: df_trackers::webhook::IssueSnapshot {
            id: "10001".into(),
            reference: "ENG-7".into(),
            title: "New title".into(),
            body: Some("New body".into()),
            state: state.into(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            updated_at: updated_at.map(str::to_string),
            state_reason: None,
            comment: None,
        },
    }
}

#[test]
fn inbound_drops_when_the_trigger_label_does_not_match() {
    let binding = binding(Provider::Github, "dark-factory");
    let event = github_issue_event("opened", &["triaged"], Some("2026-09-03T12:00:00Z"), None);

    let decision = df_trackers::sync::inbound_decision(&event, &binding, None);
    assert!(matches!(decision, df_trackers::sync::InboundDecision::Drop));
}

#[test]
fn inbound_creates_for_a_matching_issue_without_an_existing_job() {
    let binding = binding(Provider::Github, "dark-factory");
    let event = github_issue_event(
        "opened",
        &["dark-factory"],
        Some("2026-09-03T12:00:00-06:00"),
        None,
    );

    let decision = df_trackers::sync::inbound_decision(&event, &binding, None);
    match decision {
        df_trackers::sync::InboundDecision::Create {
            title,
            description,
            ticket_ref,
            remote_revision,
        } => {
            assert_eq!(title, "New title");
            assert_eq!(description.as_deref(), Some("New body"));
            assert_eq!(ticket_ref, "acme/api#17");
            assert_eq!(remote_revision.as_deref(), Some("2026-09-03T18:00:00Z"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn inbound_updates_a_matching_non_terminal_job() {
    let binding = binding(Provider::Jira, "dark-factory");
    let event = jira_issue_event(
        "In Progress",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
    );
    let existing = job(
        Status::InProgress,
        Tracker::Jira,
        "ENG-7",
        Some("2026-09-03T10:00:00Z"),
    );

    let decision = df_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    match decision {
        df_trackers::sync::InboundDecision::Update {
            title,
            description,
            remote_revision,
        } => {
            assert_eq!(title, "New title");
            assert_eq!(description.as_deref(), Some("New body"));
            assert_eq!(remote_revision.as_deref(), Some("2026-09-03T12:00:00Z"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn inbound_github_closed_event_completes_or_fails_by_state_reason() {
    let binding = binding(Provider::Github, "dark-factory");
    let complete = github_issue_event(
        "closed",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
        Some("completed"),
    );
    let fail = github_issue_event(
        "closed",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
        Some("not_planned"),
    );
    let existing = job(Status::Pending, Tracker::Github, "acme/api#17", None);

    let complete_decision =
        df_trackers::sync::inbound_decision(&complete, &binding, Some(&existing));
    let fail_decision = df_trackers::sync::inbound_decision(&fail, &binding, Some(&existing));

    assert!(matches!(
        complete_decision,
        df_trackers::sync::InboundDecision::Close {
            to: Status::Completed,
            ..
        }
    ));
    assert!(matches!(
        fail_decision,
        df_trackers::sync::InboundDecision::Close {
            to: Status::Failed,
            ..
        }
    ));
}

#[test]
fn inbound_jira_closed_vocabulary_completes_or_fails() {
    let binding = binding(Provider::Jira, "dark-factory");
    let done = jira_issue_event("Resolved", &["dark-factory"], Some("2026-09-03T12:00:00Z"));
    let failed = jira_issue_event("Rejected", &["dark-factory"], Some("2026-09-03T12:00:00Z"));
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);

    assert!(matches!(
        df_trackers::sync::inbound_decision(&done, &binding, Some(&existing)),
        df_trackers::sync::InboundDecision::Close {
            to: Status::Completed,
            ..
        }
    ));
    assert!(matches!(
        df_trackers::sync::inbound_decision(&failed, &binding, Some(&existing)),
        df_trackers::sync::InboundDecision::Close {
            to: Status::Failed,
            ..
        }
    ));
}

#[test]
fn inbound_leaves_unrecognized_jira_closed_like_states_alone() {
    let binding = binding(Provider::Jira, "dark-factory");
    let event = jira_issue_event(
        "Ready For Verification",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
    );
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);

    let decision = df_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(
        decision,
        df_trackers::sync::InboundDecision::Update { .. }
    ));
}

#[test]
fn inbound_drops_stale_or_echoed_revisions() {
    let binding = binding(Provider::Github, "dark-factory");
    let event = github_issue_event(
        "edited",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
        None,
    );
    let existing = job(
        Status::Pending,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T12:00:00Z"),
    );

    let decision = df_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(decision, df_trackers::sync::InboundDecision::Drop));
}

#[test]
fn inbound_applies_unparseable_or_missing_revisions_without_clearing_the_stored_one() {
    let binding = binding(Provider::Github, "dark-factory");
    let bad = github_issue_event("edited", &["dark-factory"], Some("not-a-timestamp"), None);
    let missing = github_issue_event("edited", &["dark-factory"], None, None);
    let existing = job(
        Status::Pending,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T12:00:00Z"),
    );

    for event in [&bad, &missing] {
        match df_trackers::sync::inbound_decision(event, &binding, Some(&existing)) {
            df_trackers::sync::InboundDecision::Update {
                remote_revision, ..
            } => {
                assert!(remote_revision.is_none());
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }
}

#[test]
fn inbound_terminal_jobs_are_left_alone() {
    let binding = binding(Provider::Github, "dark-factory");
    let event = github_issue_event(
        "edited",
        &["dark-factory"],
        Some("2026-09-03T12:00:00Z"),
        None,
    );
    let existing = job(Status::Completed, Tracker::Github, "acme/api#17", None);

    let decision = df_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(decision, df_trackers::sync::InboundDecision::NoOp));
}

#[test]
fn outbound_plans_comment_text_and_provider_specific_transition_targets() {
    let claimed = df_trackers::sync::outbound_decision(
        df_trackers::sync::JobTransition::Claimed,
        Tracker::Jira,
        None,
    );
    let completed = df_trackers::sync::outbound_decision(
        df_trackers::sync::JobTransition::Completed,
        Tracker::Github,
        None,
    );
    let failed = df_trackers::sync::outbound_decision(
        df_trackers::sync::JobTransition::Failed,
        Tracker::Jira,
        Some("boom"),
    );

    assert_eq!(claimed.comment, "Claimed.");
    assert!(matches!(
        claimed.jira_transition,
        Some(df_trackers::sync::JiraTransitionTarget::NamedStatus(ref name)) if name == "In Progress"
    ));
    assert_eq!(completed.comment, "Completed.");
    assert!(matches!(
        completed.github_close,
        Some(df_trackers::sync::GithubClose { state_reason: ref reason }) if reason == "completed"
    ));
    assert_eq!(failed.comment, "boom");
    assert!(matches!(
        failed.jira_transition,
        Some(df_trackers::sync::JiraTransitionTarget::StatusCategory(ref category)) if category == "new"
    ));
}

#[test]
fn outbound_uses_supplied_claim_comment_and_picks_matching_jira_transitions() {
    let decision = df_trackers::sync::outbound_decision(
        df_trackers::sync::JobTransition::Claimed,
        Tracker::Jira,
        Some("api-agent"),
    );
    let transitions = vec![
        df_trackers::jira::Transition {
            id: "1".into(),
            name: "To Do".into(),
            to_status: "To Do".into(),
            to_status_category: "new".into(),
        },
        df_trackers::jira::Transition {
            id: "2".into(),
            name: "Start".into(),
            to_status: "In Progress".into(),
            to_status_category: "indeterminate".into(),
        },
    ];

    let chosen = df_trackers::sync::select_jira_transition(
        decision.jira_transition.as_ref().unwrap(),
        &transitions,
    )
    .unwrap();

    assert_eq!(decision.comment, "Claimed by api-agent.");
    assert_eq!(chosen.id, "2");
}

#[test]
fn outbound_returns_none_when_no_jira_transition_matches() {
    let decision = df_trackers::sync::outbound_decision(
        df_trackers::sync::JobTransition::Completed,
        Tracker::Jira,
        Some("done"),
    );
    let transitions = vec![df_trackers::jira::Transition {
        id: "1".into(),
        name: "Backlog".into(),
        to_status: "Backlog".into(),
        to_status_category: "new".into(),
    }];

    let chosen = df_trackers::sync::select_jira_transition(
        decision.jira_transition.as_ref().unwrap(),
        &transitions,
    );

    assert!(chosen.is_none());
}
