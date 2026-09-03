//! Layer 2 — proving who the human is — end to end against a real Postgres.
//!
//! `flows.rs` covers layer 1: clients, codes, tokens, audiences. This file
//! covers the door a person walks through, and the two properties that make it
//! safe to leave open on the public internet:
//!
//! - **It answers the same way for an address that has no account**, so the
//!   login form is not a directory of an enterprise's employees.
//! - **Every credential it accepts is single-use and bounded**, so a link left
//!   in an inbox or a cookie left on a laptop stops working on a schedule
//!   rather than when someone notices.

use df_auth::crypto::Cipher;
use df_auth::error::AuthError;
use df_auth::{login, sessions, totp};
use df_core::ids::UserId;
use df_core::orgs::Role;
use df_core::Db;
use sqlx::PgPool;

const ISSUER: &str = "dark-factory";
const EMAIL: &str = "rob@acme.test";

fn cipher() -> Cipher {
    use base64::Engine;
    Cipher::from_base64_key(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

async fn fixture(pool: PgPool) -> (Db, UserId) {
    let db = Db::from_pool(pool);
    let org = db.create_org("acme", "Acme").await.unwrap();
    let user = db.upsert_user(EMAIL, Some("Rob")).await.unwrap();
    db.add_member(org.id, user.id, Role::Owner).await.unwrap();
    (db, user.id)
}

/// Enrol a confirmed authenticator and hand back the generator plus the
/// recovery codes.
///
/// Confirmation uses the *previous* step's code so the current step is still
/// unconsumed and available to log in with — using the same step for both would
/// be a replay, which is exactly what the credential refuses.
async fn enrolled(db: &Db, c: &Cipher, user: UserId) -> (totp_rs::TOTP, Vec<String>) {
    let enrollment = totp::begin_enrollment(db, c, user, EMAIL, ISSUER)
        .await
        .unwrap();
    let secret = totp_rs::Secret::Encoded(enrollment.manual_key)
        .to_bytes()
        .unwrap();
    let gen = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        secret,
        Some(ISSUER.to_string()),
        EMAIL.to_string(),
    )
    .unwrap();

    let now = chrono::Utc::now().timestamp() as u64;
    totp::confirm_enrollment(db, c, user, EMAIL, ISSUER, &gen.generate(now - 30))
        .await
        .unwrap();

    (gen, enrollment.recovery_codes)
}

fn now_code(gen: &totp_rs::TOTP) -> String {
    gen.generate(chrono::Utc::now().timestamp() as u64)
}

/// Global (org-less) audit rows for one action.
async fn audit_count(db: &Db, action: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = $1")
        .bind(action)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

// ------------------------------------------------------------ email verification
// ------------------------------------------------------------------- recovery
/// A recovery *code* is not a recovery *link*: the user still holds their
/// secret, they just cannot reach it right now. Destroying it would force a
/// re-enrollment nobody asked for.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_recovery_code_login_leaves_the_authenticator_intact(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    let (gen, codes) = enrolled(&db, &c, user).await;

    let out = login::with_recovery_code(&db, EMAIL, &codes[0], None)
        .await
        .unwrap();
    assert_eq!(out.user, user);
    assert_eq!(out.method, login::Method::RecoveryCode);
    assert!(!out.must_enroll_totp);
    assert!(totp::has_confirmed_credential(&db, user).await.unwrap());
    assert_eq!(totp::remaining_recovery_codes(&db, user).await.unwrap(), 9);

    // And the authenticator still works.
    assert!(
        login::with_totp(&db, &c, EMAIL, &now_code(&gen), ISSUER, None)
            .await
            .is_ok()
    );

    // A spent code is spent.
    let err = login::with_recovery_code(&db, EMAIL, &codes[0], None)
        .await
        .unwrap_err();
    assert_eq!(err.public(), "invalid credentials");
}

// ---------------------------------------------------------------------- login

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_totp_login_opens_a_session(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    let (gen, _) = enrolled(&db, &c, user).await;

    let out = login::with_totp(&db, &c, EMAIL, &now_code(&gen), ISSUER, None)
        .await
        .unwrap();

    assert_eq!(out.user, user);
    assert_eq!(out.method, login::Method::Totp);
    assert!(!out.must_enroll_totp);
    assert_eq!(
        sessions::resolve(&db, &out.session_token)
            .await
            .unwrap()
            .user_id,
        user
    );
    assert_eq!(audit_count(&db, "auth.login.succeeded").await, 1);
}

/// The address is case- and whitespace-insensitive, because humans type it and
/// a login that fails on a capital letter sends them to support.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_address_is_normalized(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    let (gen, _) = enrolled(&db, &c, user).await;

    let out = login::with_totp(&db, &c, "  Rob@ACME.test ", &now_code(&gen), ISSUER, None)
        .await
        .unwrap();
    assert_eq!(out.user, user);
}

/// The enumeration defense. An attacker holding a corporate directory must not
/// be able to sort it into "has an account" and "does not".
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_address_fails_exactly_like_a_known_one(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    enrolled(&db, &c, user).await;

    let known = login::with_totp(&db, &c, EMAIL, "000000", ISSUER, None)
        .await
        .unwrap_err();
    let unknown = login::with_totp(&db, &c, "nobody@acme.test", "000000", ISSUER, None)
        .await
        .unwrap_err();

    assert_eq!(known.public(), "invalid credentials");
    assert_eq!(unknown.public(), known.public());
    assert_eq!(unknown.status(), known.status());

    // Both attempts are on the record. An unknown address that left no trace
    // would make the audit trail itself the oracle.
    assert_eq!(audit_count(&db, "auth.login.failed").await, 2);
}

/// A suspended account is a *known* address, which is precisely why it must not
/// be distinguishable — "this account was disabled" confirms the address.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_disabled_account_is_indistinguishable_from_an_absent_one(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    let (gen, codes) = enrolled(&db, &c, user).await;

    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();

    let err = login::with_totp(&db, &c, EMAIL, &now_code(&gen), ISSUER, None)
        .await
        .unwrap_err();
    assert_eq!(err.public(), "invalid credentials");

    // Every other door too, not just the front one.
    let err = login::with_recovery_code(&db, EMAIL, &codes[0], None)
        .await
        .unwrap_err();
    assert_eq!(err.public(), "invalid credentials");
}

/// Throttling has to cover the addresses that do not exist, or an attacker
/// simply enumerates at full speed and only slows down once they find someone.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_address_is_throttled_like_a_real_account(pool: PgPool) {
    let (db, _user) = fixture(pool).await;
    let c = cipher();

    for _ in 0..5 {
        let _ = login::with_totp(&db, &c, "nobody@acme.test", "000000", ISSUER, None).await;
    }

    let err = login::with_totp(&db, &c, "nobody@acme.test", "000000", ISSUER, None)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::RateLimited { .. }), "got {err:?}");
}

// ------------------------------------------------------------------- sessions

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_session_dies_on_logout_and_says_so_in_the_trail(pool: PgPool) {
    let (db, user) = fixture(pool).await;
    let c = cipher();
    let (gen, _) = enrolled(&db, &c, user).await;

    let out = login::with_totp(&db, &c, EMAIL, &now_code(&gen), ISSUER, None)
        .await
        .unwrap();

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
