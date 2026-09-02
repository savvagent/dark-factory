//! End-to-end authentication flows against a real Postgres.
//!
//! The unit tests in `src/` cover the pure logic — redirect matching, PKCE,
//! drift, lockout arithmetic. These cover the parts that only exist once state
//! is involved: single-use enforcement, rotation, revocation cascades, and the
//! audience check that stands between a stolen token and someone else's queue.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use df_auth::crypto::Cipher;
use df_auth::error::AuthError;
use df_auth::{oauth, tokens, totp};
use df_core::ids::{OrgId, UserId};
use df_core::orgs::Role;
use df_core::Db;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const MIGRATIONS: &str = "../df-core/migrations";
const RESOURCE: &str = "https://mcp.dark-factory.test/mcp";
const OTHER_RESOURCE: &str = "https://mcp.someone-else.test/mcp";
const ISSUER: &str = "dark-factory";
const EMAIL: &str = "rob@acme.test";

fn cipher() -> Cipher {
    Cipher::from_base64_key(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

async fn fixture(pool: PgPool) -> (Db, UserId, OrgId) {
    let db = Db::from_pool(pool);
    let org = db.create_org("acme", "Acme").await.unwrap();
    let user = db.upsert_user(EMAIL, Some("Rob")).await.unwrap();
    db.add_member(org.id, user.id, Role::Owner).await.unwrap();
    (db, user.id, org.id)
}

/// Register a client and run an authorize request through validation.
async fn registered_client(db: &Db, redirect: &str) -> String {
    oauth::register_client(
        db,
        oauth::RegistrationRequest {
            client_name: Some("Test Agent".into()),
            redirect_uris: vec![redirect.into()],
            software_id: None,
            grant_types: None,
        },
    )
    .await
    .unwrap()
    .client_id
}

fn pkce() -> (String, String) {
    let verifier = "x".repeat(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorize_req(client_id: &str, redirect: &str, challenge: &str) -> oauth::AuthorizeRequest {
    oauth::AuthorizeRequest {
        client_id: client_id.into(),
        redirect_uri: redirect.into(),
        code_challenge: challenge.into(),
        code_challenge_method: "S256".into(),
        scopes: vec!["jobs:read".into(), "jobs:write".into()],
        resource: RESOURCE.into(),
        state: None,
    }
}

// ---------------------------------------------------------------- happy path

#[sqlx::test(migrations = "../df-core/migrations")]
async fn authorization_code_flow_end_to_end(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let redirect = "http://127.0.0.1:1455/callback";
    let client_id = registered_client(&db, redirect).await;
    let (verifier, challenge) = pkce();
    let req = authorize_req(&client_id, redirect, &challenge);

    oauth::validate_authorize(&db, &req, RESOURCE)
        .await
        .unwrap();
    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();

    let (issued, got_user, got_org) =
        oauth::redeem_code(&db, &code, &client_id, redirect, &verifier, RESOURCE)
            .await
            .unwrap();

    assert_eq!(got_user, user);
    assert_eq!(got_org, org);
    assert!(issued.refresh_token.is_some());

    let principal = tokens::introspect(&db, &issued.access_token, RESOURCE)
        .await
        .unwrap();
    assert_eq!(principal.user_id, user);
    assert_eq!(principal.org_id, org);
    assert!(principal.has_scope("jobs:write"));
    assert!(!principal.has_scope("org:admin"));
    assert!(principal.require_scope("org:admin").is_err());
}

/// A client whose loopback port moved between registration and use must still
/// complete the flow — this is the ordinary case for every CLI agent.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn ephemeral_loopback_port_still_authorizes(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let client_id = registered_client(&db, "http://127.0.0.1:1455/callback").await;
    let (verifier, challenge) = pkce();

    let moved = "http://127.0.0.1:49871/callback";
    let req = authorize_req(&client_id, moved, &challenge);
    oauth::validate_authorize(&db, &req, RESOURCE)
        .await
        .unwrap();

    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    assert!(
        oauth::redeem_code(&db, &code, &client_id, moved, &verifier, RESOURCE)
            .await
            .is_ok()
    );
}

// -------------------------------------------------- authorization code rules

/// Replaying a code is a theft signal, not a retry: the code fails **and**
/// everything already issued from it is revoked.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn authorization_code_is_single_use_and_replay_revokes(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let redirect = "http://127.0.0.1:1455/callback";
    let client_id = registered_client(&db, redirect).await;
    let (verifier, challenge) = pkce();
    let req = authorize_req(&client_id, redirect, &challenge);

    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    let (issued, _, _) = oauth::redeem_code(&db, &code, &client_id, redirect, &verifier, RESOURCE)
        .await
        .unwrap();

    // The token works before the replay.
    assert!(tokens::introspect(&db, &issued.access_token, RESOURCE)
        .await
        .is_ok());

    let err = oauth::redeem_code(&db, &code, &client_id, redirect, &verifier, RESOURCE)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidGrant(_)));

    // And is dead afterwards, because the replay revoked the family.
    let after = tokens::introspect(&db, &issued.access_token, RESOURCE).await;
    assert!(
        matches!(after, Err(AuthError::Revoked)),
        "a replayed code must revoke the tokens it already produced, got {after:?}"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn code_redemption_checks_client_redirect_and_verifier(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let redirect = "https://app.acme.test/cb";
    let client_id = registered_client(&db, redirect).await;
    let other_client = registered_client(&db, "https://evil.test/cb").await;
    let (verifier, challenge) = pkce();
    let req = authorize_req(&client_id, redirect, &challenge);

    // Wrong PKCE verifier.
    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    assert!(
        oauth::redeem_code(&db, &code, &client_id, redirect, &"y".repeat(64), RESOURCE)
            .await
            .is_err(),
        "a wrong code_verifier must not redeem"
    );

    // Wrong client.
    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    assert!(
        oauth::redeem_code(&db, &code, &other_client, redirect, &verifier, RESOURCE)
            .await
            .is_err(),
        "a code must not be redeemable by a different client"
    );

    // Wrong redirect URI.
    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    assert!(
        oauth::redeem_code(
            &db,
            &code,
            &client_id,
            "https://evil.test/cb",
            &verifier,
            RESOURCE
        )
        .await
        .is_err(),
        "a code must not redeem against a different redirect_uri"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn authorize_rejects_unregistered_redirect_and_bad_pkce(pool: PgPool) {
    let (db, _, _) = fixture(pool).await;
    let client_id = registered_client(&db, "https://app.acme.test/cb").await;
    let (_, challenge) = pkce();

    let mut req = authorize_req(&client_id, "https://evil.test/cb", &challenge);
    assert!(
        oauth::validate_authorize(&db, &req, RESOURCE)
            .await
            .is_err(),
        "an unregistered redirect_uri must be refused before any consent screen"
    );

    req.redirect_uri = "https://app.acme.test/cb".into();
    req.code_challenge_method = "plain".into();
    assert!(oauth::validate_authorize(&db, &req, RESOURCE)
        .await
        .is_err());

    req.code_challenge_method = "S256".into();
    req.resource = OTHER_RESOURCE.into();
    assert!(
        oauth::validate_authorize(&db, &req, RESOURCE)
            .await
            .is_err(),
        "a resource indicator naming another server must be refused"
    );
}

// ------------------------------------------------------- audience (RFC 8707)

/// **The confused-deputy test.** A token minted for another resource server
/// must not open this one, even though it is structurally valid, unexpired,
/// unrevoked, and belongs to a real user of a real org.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_token_for_another_resource_is_refused(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;

    let issued = tokens::issue(
        &db,
        tokens::IssueParams {
            user_id: user,
            org_id: org,
            client_id: Some("df_client_x"),
            scopes: &["jobs:read".to_string()],
            resource: OTHER_RESOURCE,
            with_refresh: false,
        },
    )
    .await
    .unwrap();

    // Valid where it was minted for.
    assert!(
        tokens::introspect(&db, &issued.access_token, OTHER_RESOURCE)
            .await
            .is_ok()
    );

    // Refused here.
    let err = tokens::introspect(&db, &issued.access_token, RESOURCE)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::WrongAudience),
        "expected WrongAudience, got {err:?}"
    );
}

/// Audience matching is exact — no prefix, no host-only, no trailing-slash
/// tolerance. Each of these is a real deployment mistake that would silently
/// widen the audience.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn audience_match_is_exact(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let issued = tokens::issue(
        &db,
        tokens::IssueParams {
            user_id: user,
            org_id: org,
            client_id: None,
            scopes: &["jobs:read".to_string()],
            resource: RESOURCE,
            with_refresh: false,
        },
    )
    .await
    .unwrap();

    for near_miss in [
        "https://mcp.dark-factory.test/mcp/",
        "https://mcp.dark-factory.test",
        "https://mcp.dark-factory.test/mcp/v2",
        "http://mcp.dark-factory.test/mcp",
        "https://mcp.dark-factory.test.evil/mcp",
    ] {
        assert!(
            matches!(
                tokens::introspect(&db, &issued.access_token, near_miss).await,
                Err(AuthError::WrongAudience)
            ),
            "{near_miss} must not be accepted as the audience"
        );
    }
}

// ------------------------------------------------------------------- refresh

/// Rotation plus reuse detection: the successor works, the predecessor does
/// not, and presenting the spent one burns the whole family.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn refresh_rotates_and_reuse_revokes_the_family(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let redirect = "http://127.0.0.1:1455/callback";
    let client_id = registered_client(&db, redirect).await;
    let (verifier, challenge) = pkce();
    let req = authorize_req(&client_id, redirect, &challenge);

    let code = oauth::issue_authorization_code(&db, &req, user, org)
        .await
        .unwrap();
    let (first, _, _) = oauth::redeem_code(&db, &code, &client_id, redirect, &verifier, RESOURCE)
        .await
        .unwrap();
    let first_refresh = first.refresh_token.clone().unwrap();

    // Rotate once.
    let (second, _, _, _) = tokens::redeem_refresh(&db, &first_refresh, &client_id, RESOURCE)
        .await
        .unwrap();
    let second_refresh = second.refresh_token.clone().unwrap();
    assert_ne!(
        first_refresh, second_refresh,
        "refresh token did not rotate"
    );
    assert!(tokens::introspect(&db, &second.access_token, RESOURCE)
        .await
        .is_ok());

    // Replay the spent one. This is theft, not a retry.
    let err = tokens::redeem_refresh(&db, &first_refresh, &client_id, RESOURCE)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidGrant(_)));

    // The attacker's successor is dead too.
    assert!(
        tokens::redeem_refresh(&db, &second_refresh, &client_id, RESOURCE)
            .await
            .is_err(),
        "reuse must revoke the whole family, not just the replayed token"
    );
    assert!(
        matches!(
            tokens::introspect(&db, &second.access_token, RESOURCE).await,
            Err(AuthError::Revoked)
        ),
        "access tokens in a compromised family must be revoked"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn refresh_is_bound_to_its_client_and_resource(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let issued = tokens::issue(
        &db,
        tokens::IssueParams {
            user_id: user,
            org_id: org,
            client_id: Some("df_client_a"),
            scopes: &["jobs:read".to_string()],
            resource: RESOURCE,
            with_refresh: true,
        },
    )
    .await
    .unwrap();
    let refresh = issued.refresh_token.unwrap();

    assert!(
        tokens::redeem_refresh(&db, &refresh, "df_client_b", RESOURCE)
            .await
            .is_err(),
        "a refresh token must not be redeemable by another client"
    );
    assert!(
        tokens::redeem_refresh(&db, &refresh, "df_client_a", OTHER_RESOURCE)
            .await
            .is_err(),
        "a refresh token must not be redeemable for another resource"
    );
    assert!(
        tokens::redeem_refresh(&db, &refresh, "df_client_a", RESOURCE)
            .await
            .is_ok()
    );
}

// ----------------------------------------------------------------- PAT + revoke

/// The compatibility path must not be the weak path: a PAT carries the same
/// claims and is subject to the same audience enforcement as an OAuth token.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn pat_is_equivalent_to_an_oauth_token(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let (pat, id) = tokens::mint_pat(
        &db,
        user,
        org,
        "copilot-cli on laptop",
        &["jobs:read".into(), "jobs:write".into()],
        RESOURCE,
        None,
    )
    .await
    .unwrap();

    assert!(
        pat.starts_with("df_pat_"),
        "PATs must be identifiable on sight"
    );

    let p = tokens::introspect(&db, &pat, RESOURCE).await.unwrap();
    assert_eq!(p.user_id, user);
    assert_eq!(p.org_id, org);
    assert_eq!(p.kind, tokens::TokenKind::Pat);
    assert!(p.has_scope("jobs:write"));

    // Same audience rule as everything else.
    assert!(matches!(
        tokens::introspect(&db, &pat, OTHER_RESOURCE).await,
        Err(AuthError::WrongAudience)
    ));

    // Listed and revocable from the console.
    let listed = tokens::list_tokens(&db, user, org).await.unwrap();
    assert!(listed.iter().any(|t| t.id == id));
    assert!(tokens::revoke_by_id(&db, user, org, id).await.unwrap());
    assert!(matches!(
        tokens::introspect(&db, &pat, RESOURCE).await,
        Err(AuthError::Revoked)
    ));
}

/// A user must not be able to revoke another user's token by guessing its id.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn revocation_is_scoped_to_the_owner(pool: PgPool) {
    let (db, user, org) = fixture(pool).await;
    let other = db.upsert_user("other@acme.test", None).await.unwrap();
    db.add_member(org, other.id, Role::Member).await.unwrap();

    let (pat, id) = tokens::mint_pat(
        &db,
        user,
        org,
        "mine",
        &["jobs:read".into()],
        RESOURCE,
        None,
    )
    .await
    .unwrap();

    assert!(
        !tokens::revoke_by_id(&db, other.id, org, id).await.unwrap(),
        "another member revoked a token that is not theirs"
    );
    assert!(tokens::introspect(&db, &pat, RESOURCE).await.is_ok());
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_token_is_refused(pool: PgPool) {
    let (db, _, _) = fixture(pool).await;
    assert!(tokens::introspect(&db, "df_at_totally-made-up", RESOURCE)
        .await
        .is_err());
}

// -------------------------------------------------------------------- TOTP

#[sqlx::test(migrations = "../df-core/migrations")]
async fn totp_enrollment_confirmation_and_login(pool: PgPool) {
    let (db, user, _) = fixture(pool).await;
    let c = cipher();

    let enrollment = totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
        .await
        .unwrap();
    assert!(enrollment.provisioning_uri.starts_with("otpauth://totp/"));
    assert_eq!(enrollment.recovery_codes.len(), 10);

    let secret = totp_rs::Secret::Encoded(enrollment.manual_key.clone())
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

    // An unconfirmed credential cannot log in.
    let now = chrono::Utc::now().timestamp() as u64;
    assert!(
        totp::verify(&db, &c, user, EMAIL, ISSUER, &gen.generate(now), None)
            .await
            .is_err(),
        "an unconfirmed TOTP credential must not authenticate"
    );

    // Confirm using the PREVIOUS step's code (accepted via the drift window), so
    // the current step is still unconsumed and can be used to log in. Using the
    // same step for both would be a replay, which is exactly what we refuse.
    totp::confirm_enrollment(&db, &c, user, EMAIL, ISSUER, &gen.generate(now - 30))
        .await
        .unwrap();

    totp::verify(&db, &c, user, EMAIL, ISSUER, &gen.generate(now), None)
        .await
        .unwrap();
}

/// The replay table is what makes a ~90-second acceptance window safe.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_totp_code_cannot_be_used_twice(pool: PgPool) {
    let (db, user, _) = fixture(pool).await;
    let c = cipher();
    let enrollment = totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
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
    let code = gen.generate(now);

    totp::confirm_enrollment(&db, &c, user, EMAIL, ISSUER, &code)
        .await
        .unwrap();

    let err = totp::verify(&db, &c, user, EMAIL, ISSUER, &code, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::TotpReplay),
        "a consumed step must be refused, got {err:?}"
    );
    // And the caller is told nothing that distinguishes it from a wrong code.
    assert_eq!(err.public(), "invalid credentials");
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn recovery_codes_are_single_use(pool: PgPool) {
    let (db, user, _) = fixture(pool).await;
    let c = cipher();
    let enrollment = totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
        .await
        .unwrap();
    let code = enrollment.recovery_codes[0].clone();

    assert_eq!(totp::remaining_recovery_codes(&db, user).await.unwrap(), 10);
    totp::consume_recovery_code(&db, user, &code, None)
        .await
        .unwrap();
    assert_eq!(totp::remaining_recovery_codes(&db, user).await.unwrap(), 9);

    assert!(
        totp::consume_recovery_code(&db, user, &code, None)
            .await
            .is_err(),
        "a recovery code must not work twice"
    );
    assert!(
        totp::consume_recovery_code(&db, user, "AAAAA-BBBBB-CCCCC-DDDDD", None)
            .await
            .is_err()
    );
}

/// Re-enrolling replaces the secret rather than leaving two live credentials.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn re_enrollment_invalidates_the_previous_secret(pool: PgPool) {
    let (db, user, _) = fixture(pool).await;
    let c = cipher();

    let first = totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
        .await
        .unwrap();
    let old_secret = totp_rs::Secret::Encoded(first.manual_key)
        .to_bytes()
        .unwrap();
    let old = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        old_secret,
        Some(ISSUER.to_string()),
        EMAIL.to_string(),
    )
    .unwrap();
    let now = chrono::Utc::now().timestamp() as u64;
    totp::confirm_enrollment(&db, &c, user, EMAIL, ISSUER, &old.generate(now))
        .await
        .unwrap();

    totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
        .await
        .unwrap();
    assert!(
        totp::verify(&db, &c, user, EMAIL, ISSUER, &old.generate(now + 60), None)
            .await
            .is_err(),
        "the superseded secret must stop working"
    );
}

// ------------------------------------------------------------------- audit

/// Every login attempt lands in the audit trail, successful or not — the failed
/// ones are what an incident responder actually needs.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn login_attempts_are_audited(pool: PgPool) {
    let (db, user, _) = fixture(pool).await;
    let c = cipher();
    let enrollment = totp::begin_enrollment(&db, &c, user, EMAIL, ISSUER)
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
    totp::confirm_enrollment(&db, &c, user, EMAIL, ISSUER, &gen.generate(now - 30))
        .await
        .unwrap();

    let _ = totp::verify(&db, &c, user, EMAIL, ISSUER, "000000", Some("203.0.113.9")).await;
    let _ = totp::verify(
        &db,
        &c,
        user,
        EMAIL,
        ISSUER,
        &gen.generate(now),
        Some("203.0.113.9"),
    )
    .await;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, ip FROM audit_events WHERE actor_user_id = $1 AND action LIKE 'auth.login%' \
         ORDER BY id",
    )
    .bind(user)
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(rows.len(), 2, "both outcomes must be recorded: {rows:?}");
    assert_eq!(rows[0].0, "auth.login.failed");
    assert_eq!(rows[1].0, "auth.login.succeeded");
    assert_eq!(rows[0].1.as_deref(), Some("203.0.113.9"));
}

/// Sanity check that the constant is actually wired: an unused MIGRATIONS
/// constant would mean these tests silently ran against no schema.
#[test]
fn migrations_path_is_the_shared_one() {
    assert!(std::path::Path::new(MIGRATIONS)
        .join("0001_identity.sql")
        .exists());
}
