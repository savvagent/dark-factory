//! Teams and invitations — the membership half of the control plane.
//!
//! The token in these tests is an arbitrary byte string, which is not a
//! shortcut: `df-core` never sees an invite token, only its hash, and treating
//! it as opaque bytes here is the same contract `df-web` holds up with a real
//! SHA-256 digest.

mod common;

use common::{db, tenant, Tenant};
use df_core::error::Error;
use df_core::ids::UserId;
use df_core::orgs::Role;
use df_core::teams::TeamPatch;
use df_core::Db;
use sqlx::PgPool;

/// A second human in the same org.
async fn member(db: &Db, t: &Tenant, email: &str, role: Role) -> UserId {
    let user = db.upsert_user(email, Some("Member")).await.unwrap();
    db.add_member(t.org, user.id, role).await.unwrap();
    user.id
}

/// Someone with an account but no membership anywhere.
async fn outsider(db: &Db, email: &str) -> UserId {
    db.upsert_user(email, Some("Outsider")).await.unwrap().id
}

fn token(seed: u8) -> Vec<u8> {
    vec![seed; 32]
}

// ------------------------------------------------------------------- teams

#[sqlx::test]
async fn teams_are_created_listed_and_patched(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let platform = tx
        .create_team("Platform", "Platform Engineering")
        .await
        .unwrap();
    tx.create_team("web", "Web").await.unwrap();

    // The slug is lowercased; the name is left exactly as typed.
    assert_eq!(platform.slug, "platform");
    assert_eq!(platform.name, "Platform Engineering");

    let listed = tx.list_teams().await.unwrap();
    assert_eq!(
        listed.iter().map(|t| t.slug.as_str()).collect::<Vec<_>>(),
        vec!["platform", "web"]
    );

    let patched = tx
        .update_team(
            platform.id,
            TeamPatch {
                name: Some("Platform".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(patched.name, "Platform");
    assert_eq!(patched.slug, "platform", "an unset field is left alone");

    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn a_team_slug_is_unique_within_an_org_and_free_in_another(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    tx.create_team("platform", "Platform").await.unwrap();
    let err = tx.create_team("Platform", "Again").await.unwrap_err();
    assert!(matches!(err, Error::TeamSlugTaken(_)), "got {err:?}");
    tx.commit().await.unwrap();

    // The same slug in another org is an entirely unrelated team.
    let mut tx = db.begin(b.org).await.unwrap();
    tx.create_team("platform", "Platform").await.unwrap();
    tx.commit().await.unwrap();
}

/// The interesting case, and the reason `delete_team` does not just delete: the
/// schema's `ON DELETE SET NULL` would turn a team-scoped repo into an org-wide
/// one, publishing it to everyone without a word.
#[sqlx::test]
async fn deleting_a_team_that_still_owns_repos_is_refused(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let team = tx.create_team("platform", "Platform").await.unwrap();
    tx.update_repo(
        t.repo,
        df_core::repos::RepoPatch {
            team_id: Some(Some(team.id)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = tx.delete_team(team.id).await.unwrap_err();
    match &err {
        Error::TeamInUse { repos } => assert!(repos.contains("api"), "should name the repo"),
        other => panic!("expected TeamInUse, got {other:?}"),
    }
    tx.commit().await.unwrap();

    // The repo is still scoped to the team — a refused delete changed nothing.
    let mut tx = db.begin(t.org).await.unwrap();
    let repo = tx.get_repo(t.repo).await.unwrap().unwrap();
    assert_eq!(repo.team_id, Some(team.id));
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn a_team_with_nothing_scoped_to_it_deletes(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let team = tx.create_team("platform", "Platform").await.unwrap();
    tx.add_team_member(team.id, t.user).await.unwrap();
    tx.delete_team(team.id).await.unwrap();
    assert!(tx.list_teams().await.unwrap().is_empty());
    // The membership went with it, by cascade.
    assert!(tx.list_user_teams(t.user).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn team_membership_requires_org_membership(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let stranger = outsider(&db, "nobody@elsewhere.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let team = tx.create_team("platform", "Platform").await.unwrap();

    let err = tx.add_team_member(team.id, stranger).await.unwrap_err();
    assert!(matches!(err, Error::NotAMember(_)), "got {err:?}");

    // And adding a real member twice is not an error — the admin's intent is
    // satisfied either way.
    tx.add_team_member(team.id, t.user).await.unwrap();
    tx.add_team_member(team.id, t.user).await.unwrap();
    assert_eq!(tx.list_team_members(team.id).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn leaving_the_org_clears_every_team_membership(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = member(&db, &t, "bob@acme.test", Role::Member).await;

    let mut tx = db.begin(t.org).await.unwrap();
    let platform = tx.create_team("platform", "Platform").await.unwrap();
    let web = tx.create_team("web", "Web").await.unwrap();
    tx.add_team_member(platform.id, bob).await.unwrap();
    tx.add_team_member(web.id, bob).await.unwrap();
    assert_eq!(tx.list_user_teams(bob).await.unwrap().len(), 2);

    assert_eq!(tx.remove_from_all_teams(bob).await.unwrap(), 2);
    assert!(tx.list_user_teams(bob).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resolving_an_unknown_team_names_the_alternatives(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    tx.create_team("platform", "Platform").await.unwrap();

    let err = tx.resolve_team("platfrom").await.unwrap_err();
    match &err {
        Error::TeamNotFound { known, .. } => assert!(known.contains("platform")),
        other => panic!("expected TeamNotFound, got {other:?}"),
    }
    tx.commit().await.unwrap();
}

// ----------------------------------------------------------------- invites

#[sqlx::test]
async fn an_invitation_grants_the_role_it_names_and_is_single_use(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = outsider(&db, "bob@acme.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let invite = tx
        .create_invite("bob@acme.test", Role::Admin, Some(t.user), &token(1))
        .await
        .unwrap();
    assert_eq!(invite.role, Role::Admin);
    assert_eq!(tx.list_invites().await.unwrap().len(), 1);

    let role = tx
        .accept_invite(&token(1), bob, "bob@acme.test")
        .await
        .unwrap();
    assert_eq!(role, Role::Admin);
    tx.commit().await.unwrap();

    assert_eq!(
        db.member_role(t.org, bob).await.unwrap(),
        Some(Role::Admin),
        "acceptance grants the invited role"
    );

    // Spent, and no longer listed.
    let mut tx = db.begin(t.org).await.unwrap();
    assert!(tx.list_invites().await.unwrap().is_empty());
    let err = tx
        .accept_invite(&token(1), bob, "bob@acme.test")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InviteInvalid), "got {err:?}");
    tx.commit().await.unwrap();
}

/// A forwarded invitation mail must not be a way into someone else's org.
#[sqlx::test]
async fn an_invitation_is_bound_to_the_address_it_was_sent_to(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let mallory = outsider(&db, "mallory@elsewhere.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    tx.create_invite("bob@acme.test", Role::Member, Some(t.user), &token(2))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .accept_invite(&token(2), mallory, "mallory@elsewhere.test")
        .await
        .unwrap_err();
    match &err {
        Error::InviteWrongAccount { invited, .. } => assert_eq!(invited, "bob@acme.test"),
        other => panic!("expected InviteWrongAccount, got {other:?}"),
    }
    // Deliberately dropped, not committed: the refusal must roll the claim back.
    drop(tx);

    assert_eq!(
        db.member_role(t.org, mallory).await.unwrap(),
        None,
        "the wrong account must not have been admitted"
    );

    // And the invitation survives for the right person.
    let bob = outsider(&db, "bob@acme.test").await;
    let mut tx = db.begin(t.org).await.unwrap();
    assert_eq!(tx.list_invites().await.unwrap().len(), 1, "still pending");
    tx.accept_invite(&token(2), bob, "BOB@acme.test")
        .await
        .expect("addresses compare case-insensitively");
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn resending_supersedes_the_live_invitation(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = outsider(&db, "bob@acme.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    tx.create_invite("bob@acme.test", Role::Member, Some(t.user), &token(3))
        .await
        .unwrap();
    tx.create_invite("bob@acme.test", Role::Member, Some(t.user), &token(4))
        .await
        .unwrap();

    assert_eq!(tx.list_invites().await.unwrap().len(), 1, "one live invite");

    let err = tx
        .accept_invite(&token(3), bob, "bob@acme.test")
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::InviteInvalid),
        "the superseded link must be dead, got {err:?}"
    );
    tx.accept_invite(&token(4), bob, "bob@acme.test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn inviting_someone_who_is_already_in_the_org_is_refused(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    member(&db, &t, "bob@acme.test", Role::Member).await;

    let mut tx = db.begin(t.org).await.unwrap();
    let err = tx
        .create_invite("BOB@acme.test", Role::Admin, Some(t.user), &token(5))
        .await
        .unwrap_err();
    match &err {
        Error::AlreadyAMember { role, .. } => assert_eq!(role, "member"),
        other => panic!("expected AlreadyAMember, got {other:?}"),
    }
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn an_expired_invitation_cannot_be_accepted(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = outsider(&db, "bob@acme.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let invite = tx
        .create_invite("bob@acme.test", Role::Member, Some(t.user), &token(6))
        .await
        .unwrap();
    sqlx::query("UPDATE org_invites SET expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(invite.id)
        .execute(tx.conn())
        .await
        .unwrap();

    assert!(tx.list_invites().await.unwrap().is_empty(), "not listed");
    let err = tx
        .accept_invite(&token(6), bob, "bob@acme.test")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InviteInvalid), "got {err:?}");
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn revoking_an_invitation_kills_the_link(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = outsider(&db, "bob@acme.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let invite = tx
        .create_invite("bob@acme.test", Role::Member, Some(t.user), &token(7))
        .await
        .unwrap();
    tx.revoke_invite(invite.id).await.unwrap();

    let err = tx
        .accept_invite(&token(7), bob, "bob@acme.test")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InviteInvalid), "got {err:?}");

    // Revoking again is a clean refusal, not a silent success.
    let err = tx.revoke_invite(invite.id).await.unwrap_err();
    assert!(matches!(err, Error::InviteInvalid), "got {err:?}");
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn peeking_reads_an_invitation_without_spending_it(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let bob = outsider(&db, "bob@acme.test").await;

    let mut tx = db.begin(t.org).await.unwrap();
    tx.create_invite("bob@acme.test", Role::Member, Some(t.user), &token(8))
        .await
        .unwrap();

    let peeked = tx.peek_invite(&token(8)).await.unwrap();
    assert_eq!(peeked.email, "bob@acme.test");
    assert!(peeked.accepted_at.is_none());

    tx.accept_invite(&token(8), bob, "bob@acme.test")
        .await
        .expect("peeking must not consume the invitation");
    tx.commit().await.unwrap();
}
