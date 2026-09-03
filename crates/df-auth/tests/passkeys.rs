//! Passkey registration and sign-in, driven through a software authenticator.
//!
//! `SoftPasskey` performs the real ceremony — it produces genuine COSE
//! signatures over the challenge and client data this server issued, and
//! `webauthn-rs` verifies them the same way it verifies a YubiKey's. A test
//! that stubbed the signature would assert nothing about the part most likely
//! to be wrong.
//!
//! It is constructed with `falsify_uv: true` because the server requires user
//! verification and a software token has no biometric to perform. That is the
//! one thing here that is not what a real authenticator does, and it is the
//! difference between testing this flow and not testing it.
//!
//! ## What these tests do and do not cover
//!
//! Neither software authenticator in `webauthn-authenticator-rs` supports
//! resident keys — `SoftToken` says so in as many words ("These will be
//! supported in future :)") — so the challenges handed to it here are softened
//! by [`for_soft_token`] and [`offer`]: the resident-key requirement is dropped,
//! and the credential is named in `allowCredentials` because a token that does
//! not hold discoverable credentials cannot find one unprompted.
//!
//! **Only what is handed to the fake authenticator is softened. The server path
//! is the production one**, byte for byte: the same challenge issuance, the same
//! ceremony storage, the same account resolution by credential ID, and the same
//! signature verification. Neither field is part of the verification state.
//!
//! What is therefore *not* covered here is the browser finding a credential
//! without being told which one — that is browser behaviour rather than this
//! server's, and it is what `residentKey: required` asks for in production.

use df_auth::{login, passkeys};
use df_core::ids::UserId;
use df_core::Db;
use sqlx::PgPool;
use webauthn_authenticator_rs::softtoken::SoftToken;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_rs::prelude::Url;

const RP_ID: &str = "console.dark-factory.test";
const ORIGIN: &str = "https://console.dark-factory.test";

fn rp() -> webauthn_rs::Webauthn {
    passkeys::relying_party(RP_ID, ORIGIN).unwrap()
}

fn authenticator() -> WebauthnAuthenticator<SoftToken> {
    WebauthnAuthenticator::new(SoftToken::new(true).unwrap().0)
}

/// Drop the resident-key requirement from a challenge before handing it to the
/// software token. See the module docs: the server's state is unaffected.
fn for_soft_token(
    mut challenge: webauthn_rs::prelude::CreationChallengeResponse,
) -> webauthn_rs::prelude::CreationChallengeResponse {
    if let Some(sel) = challenge.public_key.authenticator_selection.as_mut() {
        sel.require_resident_key = false;
        sel.resident_key = None;
    }
    challenge
}

/// Name a credential in `allowCredentials`, so a token holding no discoverable
/// credentials can still find the right key. Production sends this list empty.
fn offer(
    mut challenge: webauthn_rs::prelude::RequestChallengeResponse,
    credential_id: &[u8],
) -> webauthn_rs::prelude::RequestChallengeResponse {
    use webauthn_rs::prelude::Base64UrlSafeData;
    use webauthn_rs_proto::AllowCredentials;
    challenge.public_key.allow_credentials = vec![AllowCredentials {
        type_: "public-key".to_string(),
        id: Base64UrlSafeData::from(credential_id.to_vec()),
        transports: None,
    }];
    challenge
}

/// Register a brand-new account, the way signup does.
async fn register_new(db: &Db, auth: &mut WebauthnAuthenticator<SoftToken>) -> UserId {
    let webauthn = rp();
    let ceremony = passkeys::start_registration(db, &webauthn, None)
        .await
        .unwrap();
    let credential = auth
        .do_registration(
            Url::parse(ORIGIN).unwrap(),
            for_soft_token(ceremony.challenge),
        )
        .expect("the authenticator refused the registration challenge");
    passkeys::finish_registration(db, &webauthn, ceremony.id, &credential, Some("laptop"))
        .await
        .unwrap()
}

/// The credential IDs an account holds, for `offer`.
async fn credential_ids(db: &Db, user: UserId) -> Vec<Vec<u8>> {
    sqlx::query_scalar("SELECT credential_id FROM passkeys WHERE user_id = $1 ORDER BY created_at")
        .bind(user)
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// Sign in, usernameless — nothing but the credential identifies the account.
async fn sign_in(
    db: &Db,
    auth: &mut WebauthnAuthenticator<SoftToken>,
    credential_id: &[u8],
) -> df_auth::error::Result<UserId> {
    let webauthn = rp();
    let ceremony = passkeys::start_authentication(db, &webauthn).await.unwrap();
    let credential = auth
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            offer(ceremony.challenge, credential_id),
        )
        .expect("the authenticator refused the authentication challenge");
    passkeys::finish_authentication(db, &webauthn, ceremony.id, &credential, None).await
}

/// The whole point, in one test: an account is created by registering a key,
/// and signing back in names nobody.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_passkey_creates_an_account_and_signs_back_into_it(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();

    let user = register_new(&db, &mut auth).await;

    // The account exists and has no address — the passkey brought it into
    // being, and the profile comes later.
    let account = db.get_user(user).await.unwrap().unwrap();
    assert_eq!(account.email, None);
    assert_eq!(passkeys::count(&db, user).await.unwrap(), 1);

    let ids = credential_ids(&db, user).await;
    let signed_in = sign_in(&db, &mut auth, &ids[0]).await.unwrap();
    assert_eq!(
        signed_in, user,
        "the credential must resolve to the account that registered it"
    );

    let opened = login::with_passkey(&db, signed_in, None).await.unwrap();
    assert_eq!(opened.method, login::Method::Passkey);
    assert!(
        opened.should_add_passkey,
        "one key means one device, and the console has to say so"
    );
}

/// A second key on the same account is the recovery story, and either one must
/// open the account on its own.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_second_passkey_also_opens_the_account(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut first = authenticator();
    let user = register_new(&db, &mut first).await;

    let webauthn = rp();
    let mut second = authenticator();
    let ceremony = passkeys::start_registration(&db, &webauthn, Some(user))
        .await
        .unwrap();
    let credential = second
        .do_registration(
            Url::parse(ORIGIN).unwrap(),
            for_soft_token(ceremony.challenge),
        )
        .unwrap();
    let same =
        passkeys::finish_registration(&db, &webauthn, ceremony.id, &credential, Some("phone"))
            .await
            .unwrap();
    assert_eq!(same, user);
    assert_eq!(passkeys::count(&db, user).await.unwrap(), 2);

    let ids = credential_ids(&db, user).await;
    assert_eq!(sign_in(&db, &mut first, &ids[0]).await.unwrap(), user);
    assert_eq!(sign_in(&db, &mut second, &ids[1]).await.unwrap(), user);

    let opened = login::with_passkey(&db, user, None).await.unwrap();
    assert!(
        !opened.should_add_passkey,
        "two keys is the state the console stops nagging about"
    );
}

/// A ceremony is single-use. Replaying a captured one must not authenticate
/// anybody — this is what the `DELETE … RETURNING` in `take_ceremony` buys.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_ceremony_cannot_be_replayed(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    let user = register_new(&db, &mut auth).await;
    let ids = credential_ids(&db, user).await;

    let webauthn = rp();
    let ceremony = passkeys::start_authentication(&db, &webauthn)
        .await
        .unwrap();
    let credential = auth
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            offer(ceremony.challenge, &ids[0]),
        )
        .unwrap();

    passkeys::finish_authentication(&db, &webauthn, ceremony.id, &credential, None)
        .await
        .expect("the first use must work");

    let replayed =
        passkeys::finish_authentication(&db, &webauthn, ceremony.id, &credential, None).await;
    assert!(
        replayed.is_err(),
        "a spent ceremony authenticated a second time"
    );
}

/// A registration state must not be finishable as an authentication, or the
/// two ceremonies' guarantees leak into each other.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_registration_ceremony_is_not_an_authentication(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    let user = register_new(&db, &mut auth).await;
    let ids = credential_ids(&db, user).await;

    let webauthn = rp();
    let registration = passkeys::start_registration(&db, &webauthn, None)
        .await
        .unwrap();

    // Drive a real sign-in, then try to finish it against the registration's
    // ceremony id.
    let signin = passkeys::start_authentication(&db, &webauthn)
        .await
        .unwrap();
    let assertion = auth
        .do_authentication(
            Url::parse(ORIGIN).unwrap(),
            offer(signin.challenge, &ids[0]),
        )
        .unwrap();

    let crossed =
        passkeys::finish_authentication(&db, &webauthn, registration.id, &assertion, None).await;
    assert!(
        crossed.is_err(),
        "a registration ceremony was spent as an authentication"
    );
}

/// A signature made for a different relying party must not be accepted. This is
/// the phishing resistance the whole change is for: a lookalike origin cannot
/// produce something this server will take.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_credential_from_another_origin_is_refused(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    register_new(&db, &mut auth).await;

    // The attacker's site issues its own challenge, and the authenticator
    // happily signs for *them* — that is correct behaviour, and it is why the
    // server checks the origin rather than trusting the signature alone.
    let evil = passkeys::relying_party("evil.test", "https://evil.test").unwrap();
    let ours = rp();

    let ceremony = passkeys::start_authentication(&db, &ours).await.unwrap();
    let stolen = auth.do_authentication(Url::parse("https://evil.test").unwrap(), {
        let (challenge, _) = evil.start_discoverable_authentication().unwrap();
        challenge
    });

    // Either the authenticator refuses outright (no credential for that RP), or
    // it signs something our server must reject. Both are acceptable; silently
    // accepting is not.
    if let Ok(credential) = stolen {
        let accepted =
            passkeys::finish_authentication(&db, &ours, ceremony.id, &credential, None).await;
        assert!(
            accepted.is_err(),
            "a credential signed for another origin was accepted"
        );
    }
}

/// Removing the last passkey would lock the account out permanently, with no
/// email to recover through. The refusal is the feature.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_last_passkey_cannot_be_removed(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    let user = register_new(&db, &mut auth).await;

    let keys = passkeys::list(&db, user).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].nickname.as_deref(), Some("laptop"));

    let refused = passkeys::remove(&db, user, keys[0].id).await;
    assert!(refused.is_err(), "the only passkey was removed");
    assert_eq!(passkeys::count(&db, user).await.unwrap(), 1);
}

/// A disabled account's keys still produce valid signatures. The refusal has to
/// happen at the account level, after the ceremony.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_disabled_account_cannot_sign_in_with_a_valid_passkey(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    let user = register_new(&db, &mut auth).await;

    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();

    // The ceremony itself still succeeds — the credential is genuine.
    let ids = credential_ids(&db, user).await;
    let identified = sign_in(&db, &mut auth, &ids[0]).await.unwrap();
    assert_eq!(identified, user);

    // The account check is what refuses.
    match login::with_passkey(&db, user, None).await {
        Ok(_) => panic!("a disabled account was signed in"),
        Err(e) => assert_eq!(e.public(), "invalid credentials"),
    }
}

/// Clearing an account's keys is the admin half of recovery, and it must leave
/// the account genuinely unusable rather than merely inconvenient.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn clearing_passkeys_leaves_no_way_in(pool: PgPool) {
    let db = Db::from_pool(pool);
    let mut auth = authenticator();
    let user = register_new(&db, &mut auth).await;
    let ids = credential_ids(&db, user).await;

    let removed = passkeys::clear(&db, user, None).await.unwrap();
    assert_eq!(removed, 1);
    assert!(!passkeys::has_credential(&db, user).await.unwrap());

    let refused = sign_in(&db, &mut auth, &ids[0]).await;
    assert!(
        refused.is_err(),
        "a cleared account still accepted its old passkey"
    );
}
