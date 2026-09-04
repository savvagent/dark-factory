//! Tenant isolation.
//!
//! This file is the evidence for the product's central claim: one org cannot
//! see or touch another's data. It exercises the guarantee through the real
//! `df-core` API, against a real Postgres, with RLS enabled — the same path
//! production takes.
//!
//! Every one of these tests failed to isolate at least once during development,
//! when the pinned transaction did not yet `SET LOCAL ROLE`: Postgres exempts
//! superusers and table owners from their own row-level security policies, and
//! the connecting user is both in local development and in these very tests. A
//! policy that is not exercised by a test running as the deploying role is not
//! a policy, it is a comment.

mod common;

use common::{db, job, tenant};
use df_core::ids::{JobId, OrgId};
use df_core::jobs::JobFilter;
use df_core::messages::{InboxQuery, NewMessage};
use df_core::orgs::Role;
use df_core::repos::{RepoPatch, RepoRef};
use df_core::trackers::{resolve_binding, upsert_binding, upsert_connection, Provider};
use sqlx::PgPool;

#[sqlx::test]
async fn repos_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let seen = tx.list_repos(true).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "acme api");

    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_repos(true).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "globex api");
}

/// The same remote registered by two orgs must resolve to each org's own repo.
/// Two customers working in the same open-source repository is normal, and
/// neither may learn the other exists.
#[sqlx::test]
async fn identical_remotes_resolve_per_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:shared/lib.git").await;
    let b = tenant(&db, "globex", "https://github.com/shared/lib").await;

    let r = RepoRef {
        remote: Some("git@github.com:shared/lib.git".into()),
        ..Default::default()
    };

    let mut tx = db.begin(a.org).await.unwrap();
    let got = tx.resolve_repo(&r).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got.id, a.repo);

    let mut tx = db.begin(b.org).await.unwrap();
    let got = tx.resolve_repo(&r).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got.id, b.repo);
}

#[sqlx::test]
async fn jobs_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let secret = tx.add_job(job(&a, "acme secret work")).await.unwrap();
    tx.commit().await.unwrap();

    // B lists: sees nothing.
    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_jobs(&JobFilter::default()).await.unwrap();
    assert!(seen.is_empty(), "org B saw org A's jobs: {seen:?}");

    // B fetches A's job id directly: not found, not "forbidden" — B must not
    // even learn the id exists.
    let direct = tx.get_job(&secret.id).await;
    tx.commit().await.unwrap();
    assert!(direct.is_err(), "org B read org A's job by id");
}

#[sqlx::test]
async fn cross_org_mutation_is_refused(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let target = tx.add_job(job(&a, "acme work")).await.unwrap();
    tx.commit().await.unwrap();

    // Every mutating verb, from the wrong org.
    let mut tx = db.begin(b.org).await.unwrap();
    assert!(tx
        .update_job(&target.id, Some("pwned"), None, None, None)
        .await
        .is_err());
    assert!(tx.delete_job(&target.id).await.is_err());
    assert!(tx
        .claim_jobs(std::slice::from_ref(&target.id), b.user, None)
        .await
        .is_err());
    assert!(tx.repend_job(&target.id).await.is_err());
    let _ = tx.rollback().await;

    // A's job is untouched.
    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_job(&target.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.title, "acme work");
    assert_eq!(after.status, df_core::jobs::Status::Pending);
}

/// `update_repo` from the wrong org must not reach the row, and must not be
/// able to steal a remote either — re-pointing a remote is how you would divert
/// another tenant's agents to your own queue without ever reading their data.
#[sqlx::test]
async fn cross_org_repo_updates_are_refused(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    assert!(tx
        .update_repo(
            a.repo,
            RepoPatch {
                name: Some("pwned".into()),
                active: Some(false),
                add_remotes: vec!["git@github.com:globex/stolen.git".into()],
                ..Default::default()
            },
        )
        .await
        .is_err());
    let _ = tx.rollback().await;

    let mut tx = db.begin(a.org).await.unwrap();
    let after = tx.get_repo(a.repo).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.name, "acme api");
    assert!(after.active);
}

/// A job in org A cannot be made to depend on a job in org B, which would
/// otherwise leak B's completion state into A's `ready`/`blocked` answers.
#[sqlx::test]
async fn dependencies_cannot_cross_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    let theirs = tx.add_job(job(&b, "globex work")).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let mine = tx.add_job(job(&a, "acme work")).await.unwrap();
    let res = tx
        .set_dependencies(&mine.id, std::slice::from_ref(&theirs.id), &[])
        .await;
    let _ = tx.rollback().await;

    assert!(res.is_err(), "org A depended on org B's job");
}

#[sqlx::test]
async fn messages_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.send_message(
        a.user,
        NewMessage {
            body: "internal acme plan".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.inbox(b.user, &InboxQuery::default()).await.unwrap();
    let n = tx.unread_count(b.user).await.unwrap();
    tx.commit().await.unwrap();

    assert!(seen.is_empty(), "org B read org A's messages: {seen:?}");
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn leases_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.acquire_lease(a.repo, "main", a.user, Some("agent-a"), None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    let seen = tx.list_leases(None).await.unwrap();
    tx.commit().await.unwrap();
    assert!(seen.is_empty(), "org B saw org A's leases: {seen:?}");
}

#[sqlx::test]
async fn teams_are_invisible_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let team = tx.create_team("platform", "Platform").await.unwrap();
    tx.add_team_member(team.id, a.user).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    assert!(
        tx.list_teams().await.unwrap().is_empty(),
        "org B saw org A's teams"
    );
    assert!(tx.get_team(team.id).await.unwrap().is_none());
    assert!(tx.get_team_by_slug("platform").await.unwrap().is_none());
    assert!(
        tx.list_team_members(team.id).await.unwrap().is_empty(),
        "org B read the roster of org A's team"
    );
    assert!(tx.list_user_teams(a.user).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

/// Every mutating team operation, driven from the wrong org with a real id from
/// the right one — the shape of an attack that has guessed or leaked an id.
#[sqlx::test]
async fn cross_org_team_mutation_is_refused(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let team = tx.create_team("platform", "Platform").await.unwrap();
    tx.add_team_member(team.id, a.user).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    assert!(tx
        .update_team(
            team.id,
            df_core::teams::TeamPatch {
                name: Some("hijacked".into()),
                ..Default::default()
            },
        )
        .await
        .is_err());
    assert!(tx.delete_team(team.id).await.is_err());
    assert!(
        tx.add_team_member(team.id, b.user).await.is_err(),
        "org B put its own user on org A's team"
    );
    // A no-op rather than an error, like every other delete-shaped call — what
    // matters is that org A's roster is untouched.
    tx.remove_team_member(team.id, a.user).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let team = tx.get_team(team.id).await.unwrap().expect("team survived");
    assert_eq!(team.name, "Platform", "org B renamed org A's team");
    assert_eq!(
        tx.list_team_members(team.id).await.unwrap().len(),
        1,
        "org B emptied org A's team"
    );
    tx.commit().await.unwrap();
}

/// An invitation is a credential that grants membership of one org. A token
/// leaking across the tenant boundary would grant membership of the wrong one.
#[sqlx::test]
async fn invitations_are_invisible_and_unusable_across_orgs(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let bob = db.upsert_user("bob@acme.test", None).await.unwrap();
    let hash = vec![7u8; 32];

    let mut tx = db.begin(a.org).await.unwrap();
    let invite = tx
        .create_invite("bob@acme.test", Role::Admin, Some(a.user), &hash)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b.org).await.unwrap();
    assert!(
        tx.list_invites().await.unwrap().is_empty(),
        "org B saw org A's pending invitations"
    );
    assert!(tx.peek_invite(&hash).await.is_err());
    assert!(tx.revoke_invite(invite.id).await.is_err());
    assert!(
        tx.accept_invite(&hash, bob.id, "bob@acme.test")
            .await
            .is_err(),
        "an invitation to org A admitted its holder to org B"
    );
    tx.commit().await.unwrap();

    assert_eq!(
        db.member_role(b.org, bob.id).await.unwrap(),
        None,
        "bob joined the wrong org"
    );

    // And org A's invitation is still there, unspent.
    let mut tx = db.begin(a.org).await.unwrap();
    assert_eq!(tx.list_invites().await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

/// A transaction pinned to an org that owns nothing sees nothing — rather than,
/// say, everything. The fail-closed direction is the one that matters: a bug in
/// org resolution should produce an empty result a test will catch, never a
/// cross-tenant dump.
#[sqlx::test]
async fn unknown_org_sees_nothing(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.add_job(job(&a, "acme work")).await.unwrap();
    tx.commit().await.unwrap();

    let nobody = OrgId::new();
    let mut tx = db.begin(nobody).await.unwrap();
    assert!(tx.list_repos(true).await.unwrap().is_empty());
    assert!(tx
        .list_jobs(&JobFilter::default())
        .await
        .unwrap()
        .is_empty());
    assert!(tx.list_leases(None).await.unwrap().is_empty());
    assert!(tx.get_job(&JobId::from("job-1")).await.is_err());
    tx.commit().await.unwrap();
}

/// **The test that actually exercises RLS.**
///
/// Every other test in this file passes on the strength of guard one — the
/// explicit `org_id = $1` predicate that every `df-core` query carries. Verified
/// the obvious way: with `SET LOCAL ROLE` deleted from `Db::begin`, all eight of
/// them still passed. They prove the predicates work, which is worth proving,
/// but they say nothing about the second guard.
///
/// Guard two exists for the query that *forgets* the predicate — the one a
/// future contributor writes at 5pm. So this test issues exactly that query:
/// raw SQL with no `org_id` filter at all, inside a pinned transaction. If RLS
/// is doing its job the result is scoped anyway. If it is not, this returns both
/// orgs' rows and fails, which is the regression the whole mechanism is for.
#[sqlx::test]
async fn rls_scopes_a_query_that_forgets_the_org_predicate(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    for (t, title) in [(&a, "acme work"), (&b, "globex work")] {
        let mut tx = db.begin(t.org).await.unwrap();
        tx.add_job(job(t, title)).await.unwrap();
        tx.commit().await.unwrap();
    }

    // Deliberately unscoped: no WHERE org_id, no bind parameter, nothing.
    let mut tx = db.begin(a.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs ORDER BY title")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        titles,
        vec!["acme work".to_string()],
        "an unscoped query leaked across tenants — row-level security is not in effect"
    );

    // And the same from the other side, so a policy that happens to pin one
    // hard-coded org would still fail.
    let mut tx = db.begin(b.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs ORDER BY title")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(titles, vec!["globex work".to_string()]);
}

/// The same, for writes: an unscoped UPDATE inside a pinned transaction must not
/// reach another tenant's rows.
#[sqlx::test]
async fn rls_scopes_an_unscoped_update(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    for (t, title) in [(&a, "acme work"), (&b, "globex work")] {
        let mut tx = db.begin(t.org).await.unwrap();
        tx.add_job(job(t, title)).await.unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = db.begin(a.org).await.unwrap();
    let affected = sqlx::query("UPDATE jobs SET title = 'rewritten'")
        .execute(tx.conn())
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(
        affected, 1,
        "an unscoped UPDATE reached another tenant's rows"
    );

    let mut tx = db.begin(b.org).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM jobs")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(titles, vec!["globex work".to_string()]);
}

#[sqlx::test]
async fn rls_scopes_tracker_connections(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();

    let seen: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tracker_connections WHERE provider = 'github'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let updated = sqlx::query("UPDATE tracker_connections SET external_id = 'pwned'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    let deleted = sqlx::query("DELETE FROM tracker_connections")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();

    assert_eq!(seen, 0, "org B saw org A's tracker connection");
    assert_eq!(updated, 0, "org B updated org A's tracker connection");
    assert_eq!(deleted, 0, "org B deleted org A's tracker connection");

    let mut tx = db.begin(a.org).await.unwrap();
    let after = df_core::trackers::get_connection(&mut tx, Provider::Github)
        .await
        .unwrap()
        .expect("connection survived");
    tx.commit().await.unwrap();
    assert_eq!(after.id, connection.id);
    assert_eq!(after.external_id, "installation-1");
}

#[sqlx::test]
async fn rls_scopes_tracker_bindings(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    let binding = upsert_binding(
        &mut tx,
        a.repo,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "dark-factory",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();

    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM tracker_bindings")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let updated = sqlx::query("UPDATE tracker_bindings SET external_ref = 'pwned'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    let deleted = sqlx::query("DELETE FROM tracker_bindings")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    tx.commit().await.unwrap();

    assert_eq!(seen, 0, "org B saw org A's tracker binding");
    assert_eq!(updated, 0, "org B updated org A's tracker binding");
    assert_eq!(deleted, 0, "org B deleted org A's tracker binding");

    let mut tx = db.begin(a.org).await.unwrap();
    let after = resolve_binding(&mut tx, a.repo, Provider::Github)
        .await
        .unwrap()
        .expect("binding survived");
    tx.commit().await.unwrap();
    assert_eq!(after.id, binding.id);
    assert_eq!(after.external_ref, "acme/api");
}

// ---------------------------------------------------------------------------
// The guard on the guard.
//
// Everything above proves isolation holds *in this test database*. These prove
// the server can tell whether it holds in some *other* database — which is a
// different question, and the one a deployment gets wrong. Row-level security
// is the guard the environment can switch off: identical migrations isolate
// under one connecting role and not at all under another.
// ---------------------------------------------------------------------------

/// The happy path, and the reason it is not trivial: `#[sqlx::test]` connects as
/// the role Postgres was initialised with, which is a superuser. Isolation here
/// is real only because `SET LOCAL ROLE df_app` drops out of it, so a passing
/// assertion is evidence the role was genuinely assumed.
#[sqlx::test]
async fn isolation_verifies_on_a_healthy_database(pool: PgPool) {
    let db = db(pool);
    let report = db
        .verify_tenant_isolation()
        .await
        .expect("a freshly migrated database must enforce isolation");

    assert!(
        report.tenant_role_assumed,
        "the test database can create df_app, so it must have been assumed — \
         a false here means begin() is no longer dropping out of the superuser"
    );
    assert_eq!(report.effective_role, "df_app");
    assert!(!report.role_is_superuser && !report.role_bypasses_rls);
    assert!(
        report.tables.len() >= 13,
        "expected every tenant table from 0007_rls.sql, got {:?}",
        report.tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert!(
        report.tables.iter().all(|t| t.rls_forced),
        "every tenant table must be FORCE ROW LEVEL SECURITY — that is what \
         carries isolation on a deployment that cannot create df_app"
    );
}

/// Turning RLS off on one table must fail the check, naming that table. This is
/// the shape of a future migration that adds a tenant table and forgets the
/// `ENABLE ROW LEVEL SECURITY` half.
#[sqlx::test]
async fn isolation_refuses_a_table_with_row_level_security_disabled(pool: PgPool) {
    sqlx::query("ALTER TABLE jobs DISABLE ROW LEVEL SECURITY")
        .execute(&pool)
        .await
        .unwrap();

    let db = db(pool);
    let err = db
        .verify_tenant_isolation()
        .await
        .expect_err("a tenant table with RLS off is not isolated");

    let message = err.to_string();
    assert!(message.contains("jobs"), "{message}");
    assert!(message.contains("not enabled"), "{message}");
    assert_eq!(err.code(), "isolation_not_enforced");
}

/// A database that never ran `0007_rls.sql` must not read as healthy. Dropping
/// every policy is the same end state, and it is what a partially-applied
/// migration leaves behind.
#[sqlx::test]
async fn isolation_refuses_a_database_with_no_policies(pool: PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' \
           AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid)",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for t in &tables {
        sqlx::query(&format!("DROP POLICY {t}_tenant_isolation ON {t}"))
            .execute(&pool)
            .await
            .unwrap();
    }

    let db = db(pool);
    let err = db
        .verify_tenant_isolation()
        .await
        .expect_err("a database with no tenant policies is not isolated");
    assert!(err.to_string().contains("0007_rls.sql"), "{err}");
}

/// The verification must not leave the session pinned to `df_app`, or the
/// control plane — which runs unpinned, as the connecting role — would silently
/// lose the privileges it needs on the very next checkout from the pool.
#[sqlx::test]
async fn verifying_isolation_does_not_poison_the_pool(pool: PgPool) {
    let db = db(pool);
    db.verify_tenant_isolation().await.unwrap();

    let mut tx = db.begin_unpinned().await.unwrap();
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_ne!(role, "df_app", "verification leaked SET ROLE into the pool");

    // And the ordinary path still isolates afterwards.
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;
    let mut tx = db.begin(a.org).await.unwrap();
    assert_eq!(tx.list_repos(true).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
    let mut tx = db.begin(b.org).await.unwrap();
    assert_eq!(tx.list_repos(true).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------
// The audit trail under row-level security.
//
// These tests all issue `SET LOCAL ROLE df_app` by hand, which is not how the
// rest of the suite works and is the point: `#[sqlx::test]` connects as the
// superuser Postgres was initialised with, and a superuser bypasses row-level
// security even on a FORCE'd table. A test of an audit *policy* written the
// ordinary way would pass against no policy at all.
//
// Dropping to `df_app` makes the policies apply, which is the same thing that
// happens on a deployment connecting as a non-superuser schema owner — the
// shape where `audit_events` previously rejected every login's audit row.
// ---------------------------------------------------------------------------

const AUDIT_INSERT: &str = "INSERT INTO audit_events (org_id, actor_label, action, detail) \
                            VALUES ($1, 'someone', 'auth.login.succeeded', '{}')";

/// `Db::audit_global` writes with a NULL org from no transaction at all. Under
/// FORCE RLS that is a row the tenant policy cannot describe, so the append
/// policy has to admit the unpinned case explicitly — or every login, TOTP
/// enrolment and email verification fails to leave a trace.
#[sqlx::test]
async fn the_control_plane_can_append_audit_rows_with_no_org(pool: PgPool) {
    let db = db(pool);
    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();

    sqlx::query(AUDIT_INSERT)
        .bind(Option::<OrgId>::None)
        .execute(&mut *tx)
        .await
        .expect("the control plane must be able to record a login");
    tx.commit().await.unwrap();
}

/// The same, for an org-scoped event written before a tenant transaction exists
/// — `Db::audit_for_org`, which signup uses.
#[sqlx::test]
async fn the_control_plane_can_append_an_org_scoped_audit_row(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(&mut *tx)
        .await
        .expect("signup must be able to record an org event");
    tx.commit().await.unwrap();
}

/// A pinned transaction may not forge a row for a different org. This is the
/// half the unpinned carve-out must not have opened up.
#[sqlx::test]
async fn a_pinned_transaction_cannot_forge_another_orgs_audit_row(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let err = sqlx::query(AUDIT_INSERT)
        .bind(b.org)
        .execute(tx.conn())
        .await
        .expect_err("a pinned transaction wrote another org's audit row");
    assert!(
        err.to_string().contains("row-level security"),
        "expected an RLS refusal, got: {err}"
    );
}

/// **Append-only.** Nothing may rewrite an audit row — not the tenant role, not
/// the control plane, not the table's owner. Expressed as the absence of an
/// UPDATE policy, so a command that matches no policy touches no rows.
#[sqlx::test]
async fn audit_rows_cannot_be_rewritten(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // From inside the org that owns the row, which is the strongest position a
    // request can reach.
    let mut tx = db.begin(a.org).await.unwrap();
    let rewritten = sqlx::query("UPDATE audit_events SET action = 'tampered'")
        .execute(tx.conn())
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    tx.commit().await.unwrap();
    assert_eq!(rewritten, 0, "an audit row was rewritten");

    let mut tx = db.begin(a.org).await.unwrap();
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        actions.iter().all(|a| a != "tampered"),
        "audit trail was modified: {actions:?}"
    );
}

/// Erasing is likewise unreachable from a request. Retention is a control-plane
/// job, and the policy says so by allowing DELETE only when no org is pinned.
#[sqlx::test]
async fn audit_rows_cannot_be_erased_from_a_request(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin_unpinned().await.unwrap();
    sqlx::query("SET LOCAL ROLE df_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(AUDIT_INSERT)
        .bind(a.org)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let erased = sqlx::query("DELETE FROM audit_events")
        .execute(tx.conn())
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    tx.commit().await.unwrap();
    assert_eq!(erased, 0, "a request erased an audit row");

    let mut tx = db.begin(a.org).await.unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(remaining, 1);
}
