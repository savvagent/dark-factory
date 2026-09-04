mod common;

use common::{db, job, tenant};
use df_core::jobs::Status;
use df_core::jobs::Tracker;
use df_core::repos::NewRepo;
use sqlx::PgPool;

#[sqlx::test]
async fn get_job_by_ticket_for_repo_does_not_cross_repos(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let web = tx
        .register_repo(NewRepo {
            slug: "web".into(),
            remotes: vec!["git@github.com:acme/web.git".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    tx.create_from_ticket(
        t.repo,
        Tracker::Github,
        "acme/api#17",
        "api work",
        Some("api body"),
        Some("2026-09-03T12:00:00Z"),
    )
    .await
    .unwrap();
    let web_job = tx
        .create_from_ticket(
            web.id,
            Tracker::Github,
            "acme/api#17",
            "web work",
            Some("web body"),
            Some("2026-09-03T13:00:00Z"),
        )
        .await
        .unwrap();

    let resolved = tx
        .get_job_by_ticket_for_repo(web.id, Tracker::Github, "acme/api#17")
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(resolved.id, web_job.id);
    assert_eq!(resolved.repo_id, web.id);
    assert_eq!(resolved.title, "web work");
}

#[sqlx::test]
async fn create_from_ticket_sets_tracker_fields(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let job = tx
        .create_from_ticket(
            t.repo,
            Tracker::Jira,
            "ENG-7",
            "Sync this ticket",
            Some("body"),
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();
    let fetched = tx.get_job(&job.id).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(fetched.status, Status::Pending);
    assert_eq!(fetched.tracker, Some(Tracker::Jira));
    assert_eq!(fetched.ticket_ref.as_deref(), Some("ENG-7"));
    assert_eq!(
        fetched.remote_revision.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
    assert_eq!(fetched.metadata, serde_json::json!({}));
}

/// Two concurrent webhook deliveries for the same freshly-labelled issue can
/// each run inbound_decision, each see "no existing job" for the ticket_ref,
/// and each call create_from_ticket — the exact race
/// 0015_jobs_ticket_ref_uniqueness.sql's partial unique index exists to
/// close. This drives two real, separately-connected transactions through
/// that race (one blocks on the other's uncommitted insert, then loses the
/// unique check once it commits) and asserts the loser converges on the
/// winner's job instead of erroring or creating a second job.
#[sqlx::test]
async fn concurrent_create_from_ticket_converges_on_one_job(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let db_a = db.clone();
    let db_b = db.clone();
    let repo = t.repo;
    let org = t.org;

    let (first, second) = tokio::join!(
        async move {
            let mut tx = db_a.begin(org).await.unwrap();
            let job = tx
                .create_from_ticket(
                    repo,
                    Tracker::Github,
                    "acme/api#99",
                    "first delivery",
                    None,
                    None,
                )
                .await
                .unwrap();
            // Hold the row uncommitted briefly so the second delivery below
            // genuinely races against an in-flight insert, not an
            // already-committed one.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.commit().await.unwrap();
            job
        },
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let mut tx = db_b.begin(org).await.unwrap();
            let job = tx
                .create_from_ticket(
                    repo,
                    Tracker::Github,
                    "acme/api#99",
                    "second delivery",
                    None,
                    None,
                )
                .await
                .unwrap();
            tx.commit().await.unwrap();
            job
        }
    );

    assert_eq!(
        first.id, second.id,
        "concurrent deliveries for the same ticket produced two different jobs"
    );

    let mut tx = db.begin(t.org).await.unwrap();
    let all = tx
        .list_jobs(&df_core::jobs::JobFilter {
            repo_id: Some(t.repo),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let matching: Vec<_> = all
        .into_iter()
        .filter(|j| j.ticket_ref.as_deref() == Some("acme/api#99"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one job for the racing ticket, found {matching:?}"
    );
}

#[sqlx::test]
async fn close_from_ticket_allows_pending_and_in_progress(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let pending = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#17",
            "pending",
            None,
            None,
        )
        .await
        .unwrap();
    let completed = tx
        .close_from_ticket(
            &pending.id,
            Status::Completed,
            Some("remote complete"),
            None,
            None,
        )
        .await
        .unwrap();

    let in_progress = tx.add_job(job(&t, "claimed first")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&in_progress.id), t.user, None)
        .await
        .unwrap();
    let failed = tx
        .close_from_ticket(
            &in_progress.id,
            Status::Failed,
            None,
            Some("remote failed"),
            None,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.result.as_deref(), Some("remote complete"));
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.error.as_deref(), Some("remote failed"));
}

#[sqlx::test]
async fn close_from_ticket_rejects_terminal_jobs(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let job = tx.add_job(job(&t, "done already")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&job.id), t.user, None)
        .await
        .unwrap();
    tx.complete_job(&job.id, Some("done")).await.unwrap();

    let err = tx
        .close_from_ticket(&job.id, Status::Failed, None, Some("too late"), None)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();

    assert_eq!(err.code(), "wrong_status");
}

/// `close_from_ticket`'s `remote_revision` param folds the revision stamp
/// into the same UPDATE as the status transition (mirroring
/// `update_from_ticket`'s COALESCE), so a ticket close can never leave the
/// job's status and revision written by two separate statements that could
/// drift apart.
#[sqlx::test]
async fn close_from_ticket_stamps_remote_revision_in_the_same_statement(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#99",
            "closes with revision",
            None,
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();

    let closed = tx
        .close_from_ticket(
            &created.id,
            Status::Completed,
            Some("done"),
            None,
            Some("2026-09-04T00:00:00Z"),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(closed.status, Status::Completed);
    assert_eq!(
        closed.remote_revision.as_deref(),
        Some("2026-09-04T00:00:00Z")
    );
}

/// A `None` remote_revision on close must not clear the previously-recorded
/// one — the same self-correcting COALESCE semantics as `update_from_ticket`.
#[sqlx::test]
async fn close_from_ticket_with_no_revision_preserves_the_existing_one(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#100",
            "closes without a new revision",
            None,
            Some("2026-09-03T12:00:00Z"),
        )
        .await
        .unwrap();

    let closed = tx
        .close_from_ticket(&created.id, Status::Completed, None, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        closed.remote_revision.as_deref(),
        Some("2026-09-03T12:00:00Z")
    );
}
