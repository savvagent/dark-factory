mod common;

use common::{db, job, tenant};
use df_core::error::Error;
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
async fn link_ticket_sets_tracker_and_ticket_ref(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx.add_job(job(&t, "handmade job")).await.unwrap();
    assert_eq!(created.tracker, None);
    assert_eq!(created.ticket_ref, None);

    let linked = tx
        .link_ticket(&created.id, Tracker::Github, "acme/api#42")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(linked.tracker, Some(Tracker::Github));
    assert_eq!(linked.ticket_ref.as_deref(), Some("acme/api#42"));
}

#[sqlx::test]
async fn link_ticket_clears_stale_remote_revision_on_relink(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#1",
            "first ticket",
            None,
            Some("etag-for-ticket-1"),
        )
        .await
        .unwrap();
    assert_eq!(
        created.remote_revision.as_deref(),
        Some("etag-for-ticket-1")
    );

    let relinked = tx
        .link_ticket(&created.id, Tracker::Github, "acme/api#2")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(relinked.ticket_ref.as_deref(), Some("acme/api#2"));
    assert_eq!(relinked.remote_revision, None);
}

#[sqlx::test]
async fn link_ticket_on_a_ticket_another_live_job_holds_returns_ticket_already_linked(
    pool: PgPool,
) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let holder = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#7",
            "already linked",
            None,
            None,
        )
        .await
        .unwrap();
    let other = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();

    let err = tx
        .link_ticket(&other.id, Tracker::Github, "acme/api#7")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#7");
            assert_eq!(job, holder.id);
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
}

/// `repend_job` can revive an older terminal job without touching its
/// `created_at`, so "newest row for this ticket_ref" is not always "the live
/// row a second writer just lost a unique-violation against." Set up exactly
/// that shape — an older job revived live, and a newer-but-terminal job with
/// the same ticket_ref sitting alongside it — and confirm the conflict names
/// the actual live holder, not the newer historical one.
#[sqlx::test]
async fn link_ticket_conflict_names_the_live_holder_not_a_newer_terminal_job(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let older = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#9",
            "older job",
            None,
            None,
        )
        .await
        .unwrap();
    let claimed = tx
        .claim_jobs(std::slice::from_ref(&older.id), t.user, None)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == older.id)
        .expect("older job claimable");
    tx.complete_job(&claimed.id, Some("done")).await.unwrap();

    // Created after `older` finished, so it has a later created_at and is
    // free to reuse the same ticket_ref — the unique index only blocks live
    // rows.
    let newer_terminal = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#9",
            "newer but will terminate",
            None,
            None,
        )
        .await
        .unwrap();
    let claimed_newer = tx
        .claim_jobs(std::slice::from_ref(&newer_terminal.id), t.user, None)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == newer_terminal.id)
        .expect("newer job claimable");
    tx.fail_job(&claimed_newer.id, Some("nope")).await.unwrap();

    // Revives `older` in place — created_at is unchanged, so it stays the
    // earlier of the two rows sharing this ticket_ref, but it is once again
    // the only *live* one.
    tx.repend_job(&older.id).await.unwrap();

    let contender = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();
    let err = tx
        .link_ticket(&contender.id, Tracker::Github, "acme/api#9")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#9");
            assert_eq!(
                job, older.id,
                "must name the live holder, not the newer terminal job"
            );
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
}

#[sqlx::test]
async fn link_ticket_rejects_a_blank_ticket_ref(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let created = tx.add_job(job(&t, "handmade job")).await.unwrap();

    let err = tx
        .link_ticket(&created.id, Tracker::Github, "   ")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
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

#[sqlx::test]
async fn close_from_ticket_allows_active(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let active = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#18",
            "will be active",
            None,
            None,
        )
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&active.id), t.user, None)
        .await
        .unwrap();
    tx.activate_job(&active.id).await.unwrap();

    let completed = tx
        .close_from_ticket(
            &active.id,
            Status::Completed,
            Some("remote complete"),
            None,
            None,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(completed.status, Status::Completed);
}

/// An `active` job is still the *live* holder of its `ticket_ref` for
/// `get_live_job_by_ticket_for_repo` and the partial unique index behind it
/// (`jobs_org_repo_tracker_ticket_open_idx`) — mirrors
/// `link_ticket_on_a_ticket_another_live_job_holds_returns_ticket_already_linked`,
/// but the holder has moved past `in-progress` into `active` before the
/// second job tries to link the same ref.
#[sqlx::test]
async fn link_ticket_on_a_ticket_an_active_job_holds_returns_ticket_already_linked(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let holder = tx
        .create_from_ticket(
            t.repo,
            Tracker::Github,
            "acme/api#19",
            "already linked and active",
            None,
            None,
        )
        .await
        .unwrap();
    tx.claim_jobs(std::slice::from_ref(&holder.id), t.user, None)
        .await
        .unwrap();
    tx.activate_job(&holder.id).await.unwrap();

    let other = tx.add_job(job(&t, "wants the same ticket")).await.unwrap();
    let err = tx
        .link_ticket(&other.id, Tracker::Github, "acme/api#19")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    match err {
        Error::TicketAlreadyLinked { ticket_ref, job } => {
            assert_eq!(ticket_ref, "acme/api#19");
            assert_eq!(job, holder.id);
        }
        other => panic!("expected TicketAlreadyLinked, got {other:?}"),
    }
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
