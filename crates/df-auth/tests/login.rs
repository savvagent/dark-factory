//! Sessions — what a signed-in browser holds, and how it stops being valid.
//!
//! `passkeys.rs` covers proving who the human is; `flows.rs` covers layer 1
//! (clients, codes, tokens, audiences). What is left here is the cookie: it is
//! bounded by an idle deadline *and* a hard cap, it dies when the account is
//! disabled or the user signs out, and it is not interchangeable with a bearer
//! token. A session that outlives any of those is a laptop left in a cafe.

use df_auth::error::AuthError;
use df_auth::{login, sessions};
use df_core::ids::UserId;
use df_core::orgs::Role;
use df_core::Db;
use sqlx::PgPool;

const EMAIL: &str = "rob@acme.test";

async fn fixture(pool: PgPool) -> (Db, UserId) {
    let db = Db::from_pool(pool);
    let org = db.create_org("acme", "Acme").await.unwrap();
    let user = db.upsert_user(EMAIL, Some("Rob")).await.unwrap();
    db.add_member(org.id, user.id, Role::Owner).await.unwrap();
    (db, user.id)
}
/// Global (org-less) audit rows for one action.
async fn audit_count(db: &Db, action: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = $1")
        .bind(action)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Open a session the way a completed passkey ceremony does, without repeating
/// the ceremony itself — `passkeys.rs` covers that. These tests are about what
/// happens to the cookie afterwards.
async fn signed_in(db: &Db, user: UserId) -> login::LoggedIn {
    login::with_passkey(db, user, Some("203.0.113.7"))
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_session_dies_on_logout_and_says_so_in_the_trail(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let out = signed_in(&db, user).await;

    login::logout(&db, &out.session_token, Some("203.0.113.7"))
        .await
        .unwrap();

    let err = sessions::resolve(&db, &out.session_token)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Revoked), "got {err:?}");
    assert_eq!(audit_count(&db, "auth.logout").await, 1);

    // Logging out twice, or with a cookie that was never valid, is not an error
    // — there is nothing useful a caller could do differently.
    login::logout(&db, &out.session_token, None).await.unwrap();
    login::logout(&db, "df_ss_never-existed", None)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn revoke_all_ends_every_session_at_once(pool: PgPool) {
    let (db, user) = fixture(pool).await;

    let a = sessions::create(&db, user).await.unwrap();
    let b = sessions::create(&db, user).await.unwrap();
    assert_eq!(sessions::list(&db, user).await.unwrap().len(), 2);

    assert_eq!(sessions::revoke_all(&db, user).await.unwrap(), 2);

    for s in [&a, &b] {
        assert!(matches!(
            sessions::resolve(&db, &s.token).await.unwrap_err(),
            AuthError::Revoked
        ));
    }
    assert!(sessions::list(&db, user).await.unwrap().is_empty());
}

/// Disabling an account has to take effect now, not whenever the cookie in
/// somebody's browser happens to expire.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn disabling_a_user_kills_their_live_sessions(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let s = sessions::create(&db, user).await.unwrap();
    sessions::resolve(&db, &s.token).await.unwrap();

    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();

    let err = sessions::resolve(&db, &s.token).await.unwrap_err();
    assert!(matches!(err, AuthError::Disabled), "got {err:?}");
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_expired_session_is_refused(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let s = sessions::create(&db, user).await.unwrap();

    sqlx::query("UPDATE browser_sessions SET expires_at = now() - interval '1 second'")
        .execute(db.pool())
        .await
        .unwrap();

    assert!(matches!(
        sessions::resolve(&db, &s.token).await.unwrap_err(),
        AuthError::Expired
    ));
}

/// The two clocks: use slides the idle deadline forward, but nothing moves the
/// absolute one. Without the cap, a cookie used once a fortnight lives forever.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn use_slides_the_idle_deadline_but_never_past_the_hard_cap(pool: PgPool) {
    let (db, user) = fixture(pool).await;

    // A young session with the idle window more than half spent slides.
    let s = sessions::create(&db, user).await.unwrap();
    sqlx::query("UPDATE browser_sessions SET expires_at = now() + interval '1 day'")
        .execute(db.pool())
        .await
        .unwrap();

    let slid = sessions::resolve(&db, &s.token).await.unwrap();
    assert!(
        slid.expires_at > chrono::Utc::now() + chrono::Duration::days(13),
        "an active session should have been extended, got {}",
        slid.expires_at
    );

    // The same session, now nearly at the absolute cap, must not slide past it.
    let old = sessions::create(&db, user).await.unwrap();
    sqlx::query(
        "UPDATE browser_sessions SET created_at = now() - interval '89 days', \
                                     expires_at = now() + interval '1 day' \
         WHERE id = $1",
    )
    .bind(old.session.id)
    .execute(db.pool())
    .await
    .unwrap();

    let capped = sessions::resolve(&db, &old.token).await.unwrap();
    assert!(
        capped.expires_at
            <= capped.created_at + chrono::Duration::days(sessions::ABSOLUTE_TTL_DAYS),
        "sliding must never move a session past its absolute deadline"
    );

    // Past the cap it is dead, however recently it was used.
    sqlx::query(
        "UPDATE browser_sessions SET created_at = now() - interval '91 days', \
                                     expires_at = now() + interval '10 days' \
         WHERE id = $1",
    )
    .bind(old.session.id)
    .execute(db.pool())
    .await
    .unwrap();

    assert!(matches!(
        sessions::resolve(&db, &old.token).await.unwrap_err(),
        AuthError::Expired
    ));
}

/// A session token is not an access token and vice versa. They live in
/// different tables with different prefixes, and neither lookup should ever
/// resolve the other's credential.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_session_cookie_is_not_a_bearer_token(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let s = sessions::create(&db, user).await.unwrap();

    assert!(s.token.starts_with("df_ss_"));
    assert!(
        df_auth::tokens::introspect(&db, &s.token, "https://mcp.dark-factory.test/mcp")
            .await
            .is_err(),
        "a console cookie must not open the MCP surface"
    );
}
