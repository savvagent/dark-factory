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
        )
        .await
        .unwrap();

    let in_progress = tx.add_job(job(&t, "claimed first")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&in_progress.id), t.user, None)
        .await
        .unwrap();
    let failed = tx
        .close_from_ticket(&in_progress.id, Status::Failed, None, Some("remote failed"))
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
        .close_from_ticket(&job.id, Status::Failed, None, Some("too late"))
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();

    assert_eq!(err.code(), "wrong_status");
}
