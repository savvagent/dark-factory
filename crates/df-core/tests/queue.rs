//! Queue behaviour: job lifecycle, atomic claiming, dependencies, leases, and
//! the message channel.

mod common;

use common::{db, job, tenant};
use df_core::ids::JobId;
use df_core::jobs::{JobFilter, Status};
use df_core::messages::{InboxQuery, NewMessage};
use df_core::repos::{NewRepo, RepoPatch, RepoRef};
use sqlx::PgPool;

#[sqlx::test]
async fn job_ids_are_dense_and_per_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let a1 = tx.add_job(job(&a, "first")).await.unwrap();
    let a2 = tx.add_job(job(&a, "second")).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    let b1 = tx.add_job(job(&b, "first")).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(a1.id.as_str(), "job-1");
    assert_eq!(a2.id.as_str(), "job-2");
    // Each org starts its own count — this is what stops one customer inferring
    // another's volume from an id.
    assert_eq!(b1.id.as_str(), "job-1");
}

#[sqlx::test]
async fn lifecycle_pending_to_completed(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx.add_job(job(&t, "ship it")).await.unwrap();
    assert_eq!(j.status, Status::Pending);
    assert_eq!(j.attempts, 0);

    let claimed = tx
        .claim_jobs(
            std::slice::from_ref(&j.id),
            t.user,
            Some("claude-code@laptop"),
        )
        .await
        .unwrap();
    assert_eq!(claimed[0].status, Status::InProgress);
    assert_eq!(claimed[0].attempts, 1);
    assert_eq!(
        claimed[0].claimed_by_label.as_deref(),
        Some("claude-code@laptop")
    );

    let done = tx.complete_job(&j.id, Some("merged in #12")).await.unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(done.result.as_deref(), Some("merged in #12"));
    assert!(done.completed_at.is_some());
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn completing_an_unclaimed_job_is_refused(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx.add_job(job(&t, "not started")).await.unwrap();
    let err = tx.complete_job(&j.id, Some("lying")).await.unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(err.code(), "wrong_status");
}

#[sqlx::test]
async fn repend_preserves_attempts(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let j = tx.add_job(job(&t, "flaky")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&j.id), t.user, None)
        .await
        .unwrap();
    tx.fail_job(&j.id, Some("CI red")).await.unwrap();

    let again = tx.repend_job(&j.id).await.unwrap();
    assert_eq!(again.status, Status::Pending);
    assert_eq!(again.attempts, 1, "attempts must survive a repend");
    assert!(again.error.is_none());
    assert!(again.claimed_by.is_none());

    // The second claim increments to 2, so a job that keeps coming back is
    // visible as such rather than looking fresh every time.
    let reclaimed = tx
        .claim_jobs(std::slice::from_ref(&j.id), t.user, None)
        .await
        .unwrap();
    assert_eq!(reclaimed[0].attempts, 2);
    tx.commit().await.unwrap();
}

/// A batch claim is all-or-nothing. A partial claim would leave an agent
/// believing it owns work it does not — the exact race this queue prevents.
#[sqlx::test]
async fn claim_is_all_or_nothing(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let a = tx.add_job(job(&t, "a")).await.unwrap();
    let b = tx.add_job(job(&t, "b")).await.unwrap();
    tx.commit().await.unwrap();

    // Someone else takes `b` first.
    let mut tx = db.begin(t.org).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&b.id), t.user, Some("other"))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // The batch containing it must fail entirely, leaving `a` claimable.
    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .claim_jobs(&[a.id.clone(), b.id.clone()], t.user, Some("me"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wrong_status");
    tx.rollback().await.unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let still = tx.get_job(&a.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        still.status,
        Status::Pending,
        "a partial claim leaked through"
    );
}

#[sqlx::test]
async fn claiming_a_missing_job_names_it(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let a = tx.add_job(job(&t, "a")).await.unwrap();
    let err = tx
        .claim_jobs(&[a.id.clone(), JobId::from("job-999")], t.user, None)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();

    assert_eq!(err.code(), "job_not_found");
    assert!(
        err.to_string().contains("job-999"),
        "the error must name the missing job so the agent can fix its request: {err}"
    );
}

#[sqlx::test]
async fn dependencies_gate_ready_and_claim(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let first = tx.add_job(job(&t, "migration")).await.unwrap();
    let second = tx.add_job(job(&t, "use the new column")).await.unwrap();
    tx.set_dependencies(&second.id, std::slice::from_ref(&first.id), &[])
        .await
        .unwrap();

    let ready: Vec<String> = tx
        .ready(None)
        .await
        .unwrap()
        .into_iter()
        .map(|j| j.id.0)
        .collect();
    let blocked: Vec<String> = tx
        .blocked(None)
        .await
        .unwrap()
        .into_iter()
        .map(|j| j.id.0)
        .collect();
    assert_eq!(ready, vec![first.id.0.clone()]);
    assert_eq!(blocked, vec![second.id.0.clone()]);
    tx.commit().await.unwrap();

    // Claiming a blocked job is refused even when asked for directly. Rolled
    // back on its own so the refusal cannot be confused with the setup being
    // discarded.
    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .claim_jobs(std::slice::from_ref(&second.id), t.user, None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wrong_status");
    tx.rollback().await.unwrap();

    // Once the dependency completes, the dependent becomes ready.
    let mut tx = db.begin(t.org).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&first.id), t.user, None)
        .await
        .unwrap();
    tx.complete_job(&first.id, Some("done")).await.unwrap();
    let ready: Vec<String> = tx
        .ready(None)
        .await
        .unwrap()
        .into_iter()
        .map(|j| j.id.0)
        .collect();
    assert_eq!(ready, vec![second.id.0.clone()]);
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn dependency_cycles_are_rejected(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let a = tx.add_job(job(&t, "a")).await.unwrap();
    let b = tx.add_job(job(&t, "b")).await.unwrap();
    let c = tx.add_job(job(&t, "c")).await.unwrap();

    // Direct self-dependency.
    assert_eq!(
        tx.set_dependencies(&a.id, std::slice::from_ref(&a.id), &[])
            .await
            .unwrap_err()
            .code(),
        "dependency_cycle"
    );

    // Indirect: a → b → c → a.
    tx.set_dependencies(&a.id, std::slice::from_ref(&b.id), &[])
        .await
        .unwrap();
    tx.set_dependencies(&b.id, std::slice::from_ref(&c.id), &[])
        .await
        .unwrap();
    let err = tx
        .set_dependencies(&c.id, std::slice::from_ref(&a.id), &[])
        .await
        .unwrap_err();
    assert_eq!(err.code(), "dependency_cycle");
    tx.rollback().await.unwrap();
}

#[sqlx::test]
async fn stats_counts_blocked_separately(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let a = tx.add_job(job(&t, "a")).await.unwrap();
    let b = tx.add_job(job(&t, "b")).await.unwrap();
    tx.set_dependencies(&b.id, std::slice::from_ref(&a.id), &[])
        .await
        .unwrap();
    let c = tx.add_job(job(&t, "c")).await.unwrap();
    tx.claim_jobs(std::slice::from_ref(&c.id), t.user, None)
        .await
        .unwrap();

    let s = tx.stats(None).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(s.total, 3);
    assert_eq!(s.pending, 2);
    assert_eq!(s.in_progress, 1);
    // `blocked` is a subset of `pending`, not a separate status.
    assert_eq!(s.blocked, 1);
}

// --------------------------------------------------------------------- repos

#[sqlx::test]
async fn repo_resolves_from_any_remote_spelling(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    for spelling in [
        "git@github.com:acme/api.git",
        "https://github.com/acme/api",
        "https://github.com/acme/api.git",
        "ssh://git@github.com:22/acme/api.git",
    ] {
        let mut tx = db.begin(t.org).await.unwrap();
        let got = tx
            .resolve_repo(&RepoRef {
                remote: Some(spelling.into()),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{spelling} failed to resolve: {e}"));
        tx.commit().await.unwrap();
        assert_eq!(got.id, t.repo, "{spelling}");
    }
}

/// An unresolvable repo must error with the registered slugs, never fall back to
/// some other repo — queueing work against the wrong repository is a silent,
/// expensive failure.
#[sqlx::test]
async fn unresolvable_repo_errors_helpfully(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .resolve_repo(&RepoRef {
            remote: Some("git@github.com:acme/unknown.git".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    assert_eq!(err.code(), "repo_unresolved");
    let msg = err.to_string();
    assert!(msg.contains("api"), "must list registered slugs: {msg}");
    assert!(
        msg.contains("register_repo"),
        "must say what to call next: {msg}"
    );

    // A mistyped slug is the commonest way to get here, and it has to be just
    // as informative as a remote that matched nothing. It is the one the caller
    // can actually fix from the answer.
    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .resolve_repo(&RepoRef {
            slug: Some("apo".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    assert_eq!(err.code(), "repo_unresolved");
    let msg = err.to_string();
    assert!(msg.contains("apo"), "must repeat what was asked for: {msg}");
    assert!(msg.contains("api"), "must list registered slugs: {msg}");

    // And a slug that misses stops there. An agent typically passes its
    // checkout's remote alongside whatever repo it was told to use; falling
    // back to the remote on a typo would quietly queue the work against the
    // repository the agent happens to be sitting in.
    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .resolve_repo(&RepoRef {
            slug: Some("apo".into()),
            remote: Some("git@github.com:acme/api.git".into()),
        })
        .await
        .unwrap_err();
    tx.commit().await.unwrap();

    assert_eq!(err.code(), "repo_unresolved");
    assert!(
        err.to_string().contains("apo"),
        "an explicit slug wins over a remote, even when it misses: {err}"
    );
}

#[sqlx::test]
async fn a_remote_cannot_be_claimed_by_two_repos(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .register_repo(NewRepo {
            slug: "api-mirror".into(),
            // Same repo, different spelling — must be caught after normalization.
            remotes: vec!["https://github.com/acme/api".into()],
            ..Default::default()
        })
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(err.code(), "remote_taken");
}

// -------------------------------------------------------------------- leases

#[sqlx::test]
async fn a_second_agent_cannot_take_a_held_lease(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let other = db.upsert_user("other@acme.test", None).await.unwrap();
    db.add_member(t.org, other.id, df_core::orgs::Role::Member)
        .await
        .unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    tx.acquire_lease(t.repo, "main", t.user, Some("agent-a"), None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .acquire_lease(t.repo, "main", other.id, Some("agent-b"), None, None)
        .await
        .unwrap_err();
    tx.rollback().await.unwrap();

    assert_eq!(err.code(), "lease_held");
    assert!(
        err.to_string().contains("agent-a"),
        "the error must name the holder so the agent can go elsewhere or ask: {err}"
    );
}

/// Different branches of the same repo are independent — two agents on separate
/// worktrees is the normal case, not a collision.
#[sqlx::test]
async fn leases_are_per_branch(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let other = db.upsert_user("other@acme.test", None).await.unwrap();
    db.add_member(t.org, other.id, df_core::orgs::Role::Member)
        .await
        .unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    tx.acquire_lease(t.repo, "main", t.user, Some("a"), None, None)
        .await
        .unwrap();
    tx.acquire_lease(t.repo, "feature/x", other.id, Some("b"), None, None)
        .await
        .unwrap();
    let live = tx.list_leases(Some(t.repo)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live.len(), 2);
}

/// Re-acquiring your own lease renews it rather than failing, so an agent that
/// lost track of its own state converges instead of deadlocking against itself.
#[sqlx::test]
async fn reacquiring_your_own_lease_renews_it(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let first = tx
        .acquire_lease(t.repo, "main", t.user, Some("a"), None, Some(120))
        .await
        .unwrap();
    let second = tx
        .acquire_lease(t.repo, "main", t.user, Some("a"), None, Some(3600))
        .await
        .unwrap();
    let live = tx.list_leases(Some(t.repo)).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        first.id, second.id,
        "renewal must not create a second lease"
    );
    assert!(second.expires_at > first.expires_at);
    assert_eq!(live.len(), 1);
}

#[sqlx::test]
async fn only_the_holder_can_release(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let other = db.upsert_user("other@acme.test", None).await.unwrap();
    db.add_member(t.org, other.id, df_core::orgs::Role::Member)
        .await
        .unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let lease = tx
        .acquire_lease(t.repo, "main", t.user, Some("a"), None, None)
        .await
        .unwrap();
    assert_eq!(
        tx.release_lease(lease.id, other.id)
            .await
            .unwrap_err()
            .code(),
        "lease_not_held"
    );
    tx.release_lease(lease.id, t.user).await.unwrap();
    assert!(tx.list_leases(None).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

// ------------------------------------------------------------------ messages

#[sqlx::test]
async fn directed_messages_are_private_within_the_org(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = db.upsert_user("bob@acme.test", None).await.unwrap();
    let eve = db.upsert_user("eve@acme.test", None).await.unwrap();
    for u in [bob.id, eve.id] {
        db.add_member(t.org, u, df_core::orgs::Role::Member)
            .await
            .unwrap();
    }

    let mut tx = db.begin(t.org).await.unwrap();
    tx.send_message(
        t.user,
        NewMessage {
            body: "for bob only".into(),
            recipient_user_id: Some(bob.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.send_message(
        t.user,
        NewMessage {
            body: "everyone".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let bobs: Vec<String> = tx
        .inbox(bob.id, &InboxQuery::default())
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.body)
        .collect();
    let eves: Vec<String> = tx
        .inbox(eve.id, &InboxQuery::default())
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.body)
        .collect();
    tx.commit().await.unwrap();

    assert_eq!(bobs, vec!["for bob only", "everyone"]);
    assert_eq!(eves, vec!["everyone"], "eve read a directed message");
}

/// Your own messages are never unread to you, and the cursor cannot be advanced
/// past the newest message — otherwise a careless ack would suppress messages
/// that have not been written yet.
#[sqlx::test]
async fn unread_excludes_self_and_ack_is_clamped(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = db.upsert_user("bob@acme.test", None).await.unwrap();
    db.add_member(t.org, bob.id, df_core::orgs::Role::Member)
        .await
        .unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    tx.send_message(
        t.user,
        NewMessage {
            body: "mine".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let theirs = tx
        .send_message(
            bob.id,
            NewMessage {
                body: "theirs".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        tx.unread_count(t.user).await.unwrap(),
        1,
        "own message counted as unread"
    );
    assert_eq!(tx.unread_count(bob.id).await.unwrap(), 1);

    let landed = tx.ack_messages(t.user, i64::MAX).await.unwrap();
    assert_eq!(
        landed, theirs.id,
        "ack was not clamped to the newest message"
    );
    assert_eq!(tx.unread_count(t.user).await.unwrap(), 0);

    // A later message is still unread despite the over-large ack.
    tx.send_message(
        bob.id,
        NewMessage {
            body: "later".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(tx.unread_count(t.user).await.unwrap(), 1);
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn oversized_and_empty_bodies_are_refused(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    assert_eq!(
        tx.send_message(
            t.user,
            NewMessage {
                body: "   ".into(),
                ..Default::default()
            }
        )
        .await
        .unwrap_err()
        .code(),
        "invalid_argument"
    );
    let huge = "x".repeat(df_core::messages::MAX_BODY_LEN + 1);
    assert_eq!(
        tx.send_message(
            t.user,
            NewMessage {
                body: huge,
                ..Default::default()
            }
        )
        .await
        .unwrap_err()
        .code(),
        "invalid_argument"
    );
    tx.rollback().await.unwrap();
}

#[sqlx::test]
async fn list_jobs_filters_by_repo(pool: PgPool) {
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
    tx.add_job(job(&t, "api work")).await.unwrap();
    tx.add_job(df_core::jobs::NewJob {
        repo_id: web.id,
        title: "web work".into(),
        ..Default::default()
    })
    .await
    .unwrap();

    let only_web = tx
        .list_jobs(&JobFilter {
            repo_id: Some(web.id),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(only_web.len(), 1);
    assert_eq!(only_web[0].title, "web work");
}

/// A patch touches only what it names. A caller written against three fields
/// must not blank the two it has never heard of.
#[sqlx::test]
async fn update_repo_leaves_unnamed_fields_alone(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let before = tx.get_repo(t.repo).await.unwrap().unwrap();
    assert_eq!(before.default_branch, "main");

    let after = tx
        .update_repo(
            t.repo,
            RepoPatch {
                default_branch: Some("trunk".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(after.default_branch, "trunk");
    assert_eq!(after.name, before.name, "name was not in the patch");
    assert_eq!(after.slug, before.slug);
    assert!(after.active);
}

/// Added remotes resolve afterwards, and a remote already claimed by a sibling
/// repo is refused by name rather than silently re-pointed.
#[sqlx::test]
async fn update_repo_adds_remotes_and_refuses_stolen_ones(pool: PgPool) {
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

    tx.update_repo(
        t.repo,
        RepoPatch {
            add_remotes: vec!["https://gitlab.com/acme/api-mirror".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let resolved = tx
        .resolve_repo(&RepoRef {
            remote: Some("git@gitlab.com:acme/api-mirror.git".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(resolved.id, t.repo, "the mirror must reach the same repo");

    let err = tx
        .update_repo(
            web.id,
            RepoPatch {
                add_remotes: vec!["git@github.com:acme/api.git".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("api"),
        "the error must name the repo already holding the remote: {err}"
    );
    let _ = tx.rollback().await;
}
