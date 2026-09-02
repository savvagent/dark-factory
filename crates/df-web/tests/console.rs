//! The console API, end to end against a real Postgres.
//!
//! These drive the assembled router, so a test failing here means a real client
//! would have failed the same way. The properties under test are the ones a
//! unit test cannot reach: that the onboarding sequence actually completes, that
//! an org you are not in is indistinguishable from one that does not exist, that
//! a role boundary holds at the HTTP edge, and that removing someone actually
//! disconnects their agents.

mod common;

use common::{add_member, harness, link_token, now_code, onboard, org_with_owner, Call};
use df_core::orgs::Role;
use http::StatusCode;
use sqlx::PgPool;

// ------------------------------------------------------------- onboarding

/// The whole front door, in the order a person meets it. If this breaks, the
/// product has no first five minutes.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_new_user_signs_up_enrols_and_registers_a_repo(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;

    // The verification mail went to the right place and carried a link.
    let mail = h.mailer.sent();
    assert_eq!(mail.len(), 1, "expected exactly one verification mail");
    assert_eq!(mail[0].to, "rob@acme.test");
    assert!(mail[0]
        .text
        .contains("https://console.dark-factory.test/verify?token="));

    let me = Call::get("/api/me")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    me.expect(StatusCode::OK);
    assert_eq!(me.body["user"]["email"], "rob@acme.test");
    assert_eq!(me.body["mustEnrollTotp"], false, "enrollment was confirmed");
    assert_eq!(me.body["recoveryCodesRemaining"], 10);
    assert!(me.body["orgs"].as_array().unwrap().is_empty());

    org_with_owner(&h, "acme", &rob).await;

    let registered = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({
            "slug": "api",
            "name": "Acme API",
            "remotes": ["git@github.com:acme/api.git"],
        }))
        .send(&h.router)
        .await;
    registered.expect(StatusCode::CREATED);
    assert_eq!(
        registered.body["provider"], "github",
        "inferred from the remote"
    );

    let repos = Call::get("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    repos.expect(StatusCode::OK);
    assert_eq!(repos.body.as_array().unwrap().len(), 1);
}

/// The bootstrap rule from `routes::auth`: a verification link opens a session
/// only while the account has no second factor. Once one exists, the link marks
/// the address verified and nothing more — a mail opened later on a phone must
/// not be a way in.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_verification_link_stops_opening_sessions_once_totp_exists(pool: PgPool) {
    let h = harness(pool);
    onboard(&h, "rob@acme.test").await;

    h.mailer.clear();
    Call::post("/api/auth/link")
        .json(serde_json::json!({ "email": "rob@acme.test", "purpose": "verify" }))
        .send(&h.router)
        .await
        .expect(StatusCode::ACCEPTED);

    let token = link_token(&h.mailer.last().unwrap());
    let verified = Call::post("/api/auth/verify")
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await;
    verified.expect(StatusCode::OK);

    assert_eq!(verified.body["emailVerified"], true);
    assert_eq!(verified.body["signedIn"], false);
    assert!(
        verified.session_cookie().is_none(),
        "an account with a confirmed authenticator must not be signed in by an \
         emailed link alone"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_link_is_single_use(pool: PgPool) {
    let h = harness(pool);
    Call::post("/api/auth/signup")
        .json(serde_json::json!({ "email": "rob@acme.test" }))
        .send(&h.router)
        .await
        .expect(StatusCode::ACCEPTED);

    let token = link_token(&h.mailer.last().unwrap());
    let body = serde_json::json!({ "token": token });

    Call::post("/api/auth/verify")
        .json(body.clone())
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let replayed = Call::post("/api/auth/verify")
        .json(body)
        .send(&h.router)
        .await;
    replayed.expect(StatusCode::BAD_REQUEST);
    assert_eq!(replayed.error_code(), Some("credential_expired"));
}

/// Signup and the link endpoint must answer identically for an address that
/// exists and one that does not — anything else is an account-enumeration
/// oracle reachable without credentials.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn signup_and_link_do_not_reveal_whether_an_address_is_known(pool: PgPool) {
    let h = harness(pool);
    onboard(&h, "rob@acme.test").await;
    h.mailer.clear();

    let known = Call::post("/api/auth/link")
        .json(serde_json::json!({ "email": "rob@acme.test", "purpose": "recover" }))
        .send(&h.router)
        .await;
    let unknown = Call::post("/api/auth/link")
        .json(serde_json::json!({ "email": "nobody@acme.test", "purpose": "recover" }))
        .send(&h.router)
        .await;

    assert_eq!(known.status, unknown.status);
    assert_eq!(
        known.body, unknown.body,
        "the response distinguishes a known address from an unknown one"
    );

    // And only the real address was actually mailed.
    let sent = h.mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "rob@acme.test");
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_recovery_link_resets_the_authenticator_and_signs_in(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    h.mailer.clear();

    Call::post("/api/auth/link")
        .json(serde_json::json!({ "email": "rob@acme.test", "purpose": "recover" }))
        .send(&h.router)
        .await
        .expect(StatusCode::ACCEPTED);

    let recovered = Call::post("/api/auth/recover")
        .json(serde_json::json!({ "token": link_token(&h.mailer.last().unwrap()) }))
        .send(&h.router)
        .await;
    recovered.expect(StatusCode::OK);

    assert_eq!(recovered.body["mustEnrollTotp"], true);
    let session = recovered
        .session_cookie()
        .expect("recovery opens a session");

    // The old authenticator is gone.
    let refused = Call::post("/api/auth/login")
        .json(serde_json::json!({ "email": "rob@acme.test", "code": now_code(&rob.totp) }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert_eq!(refused.error_code(), Some("invalid_credentials"));

    // And the session it opened is good enough to enrol a new one.
    common::enroll(&h, &session, "rob@acme.test").await;
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn signing_in_and_out_works_and_a_dead_cookie_is_refused(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;

    let signed_in = Call::post("/api/auth/login")
        .json(serde_json::json!({ "email": "rob@acme.test", "code": now_code(&rob.totp) }))
        .send(&h.router)
        .await;
    signed_in.expect(StatusCode::OK);
    let session = signed_in.session_cookie().unwrap();

    Call::get("/api/me")
        .with_session(&session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let logged_out = Call::post("/api/auth/logout")
        .with_session(&session)
        .send(&h.router)
        .await;
    logged_out.expect(StatusCode::NO_CONTENT);
    assert!(
        logged_out
            .headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap().contains("Max-Age=0")),
        "logout must clear the cookie"
    );

    let after = Call::get("/api/me")
        .with_session(&session)
        .send(&h.router)
        .await;
    after.expect(StatusCode::UNAUTHORIZED);
    assert_eq!(after.error_code(), Some("unauthenticated"));
}

/// Login failures must be one answer whatever actually went wrong.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn login_failures_are_indistinguishable(pool: PgPool) {
    let h = harness(pool);
    onboard(&h, "rob@acme.test").await;

    let wrong_code = Call::post("/api/auth/login")
        .json(serde_json::json!({ "email": "rob@acme.test", "code": "000000" }))
        .send(&h.router)
        .await;
    let unknown_address = Call::post("/api/auth/login")
        .json(serde_json::json!({ "email": "nobody@acme.test", "code": "000000" }))
        .send(&h.router)
        .await;

    assert_eq!(wrong_code.status, unknown_address.status);
    assert_eq!(
        wrong_code.body, unknown_address.body,
        "a wrong code and an unknown address must look identical"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn signing_out_everywhere_ends_every_session(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;

    let second = Call::post("/api/auth/login")
        .json(serde_json::json!({ "email": "rob@acme.test", "code": now_code(&rob.totp) }))
        .send(&h.router)
        .await
        .session_cookie()
        .unwrap();

    let listed = Call::get("/api/me/sessions")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert_eq!(listed.body.as_array().unwrap().len(), 2);

    Call::delete("/api/me/sessions")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    for session in [&rob.session, &second] {
        Call::get("/api/me")
            .with_session(session)
            .send(&h.router)
            .await
            .expect(StatusCode::UNAUTHORIZED);
    }
}

// ------------------------------------------------------------------- orgs

/// The isolation property, at the HTTP edge: an org you are not in must be
/// indistinguishable from one that does not exist. A `403` here and a `404`
/// there turns any account into a customer directory.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn another_orgs_data_is_not_merely_forbidden_it_is_invisible(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let mallory = onboard(&h, "mallory@evil.test").await;

    org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "evil", &mallory).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "remotes": ["git@github.com:acme/api.git"] }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let real_org = Call::get("/api/orgs/acme/repos")
        .with_session(&mallory.session)
        .send(&h.router)
        .await;
    let imaginary_org = Call::get("/api/orgs/no-such-org/repos")
        .with_session(&mallory.session)
        .send(&h.router)
        .await;

    real_org.expect(StatusCode::NOT_FOUND);
    assert_eq!(
        real_org.status, imaginary_org.status,
        "an org you are not in must not be distinguishable from one that does not exist"
    );
    assert_eq!(real_org.error_code(), imaginary_org.error_code());
    assert!(
        !real_org.text.contains("api"),
        "the refusal leaked the other org's contents"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn creating_an_org_needs_a_verified_address(pool: PgPool) {
    let h = harness(pool);

    // Signed up but never verified: sign in is impossible, so reach the state
    // the endpoint guards by giving this account a session another way.
    Call::post("/api/auth/signup")
        .json(serde_json::json!({ "email": "rob@acme.test" }))
        .send(&h.router)
        .await
        .expect(StatusCode::ACCEPTED);

    let user =
        h.db.get_user_by_email("rob@acme.test")
            .await
            .unwrap()
            .unwrap();
    let session = df_auth::sessions::create(&h.db, user.id)
        .await
        .unwrap()
        .token;

    let refused = Call::post("/api/orgs")
        .with_session(&session)
        .json(serde_json::json!({ "slug": "acme", "name": "Acme" }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::FORBIDDEN);
    assert!(refused.text.contains("confirm your email"));
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_member_cannot_do_what_an_admin_can(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;

    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Member).await;

    // A member reads.
    Call::get("/api/orgs/acme/members")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
    Call::get("/api/orgs/acme/teams")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    // And does not write.
    for call in [
        Call::post("/api/orgs/acme/teams").json(serde_json::json!({ "slug": "platform" })),
        Call::post("/api/orgs/acme/repos").json(serde_json::json!({ "slug": "api" })),
        Call::post("/api/orgs/acme/invites").json(serde_json::json!({ "email": "eve@acme.test" })),
    ] {
        let refused = call.with_session(&bob.session).send(&h.router).await;
        refused.expect(StatusCode::FORBIDDEN);
        assert!(
            refused.text.contains("you are a member"),
            "the refusal should say what role you actually hold: {}",
            refused.text
        );
    }

    // The audit log is admin-only, unlike the rest of the reads.
    Call::get("/api/orgs/acme/audit")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::FORBIDDEN);
}

/// An org with no owner cannot be administered by anyone, and only someone with
/// database access could repair it.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_last_owner_cannot_be_removed_or_demoted(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Admin).await;

    let demote = Call::patch(format!("/api/orgs/acme/members/{}", rob.user))
        .with_session(&rob.session)
        .json(serde_json::json!({ "role": "member" }))
        .send(&h.router)
        .await;
    demote.expect(StatusCode::CONFLICT);
    assert_eq!(demote.error_code(), Some("last_owner"));

    let leave = Call::delete(format!("/api/orgs/acme/members/{}", rob.user))
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    leave.expect(StatusCode::CONFLICT);

    // With a second owner in place, both become possible.
    Call::patch(format!("/api/orgs/acme/members/{}", bob.user))
        .with_session(&rob.session)
        .json(serde_json::json!({ "role": "owner" }))
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    Call::delete(format!("/api/orgs/acme/members/{}", rob.user))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);
}

/// An admin who could promote themselves to owner is an admin with owner
/// powers one request away.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn only_an_owner_may_create_another_owner(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Admin).await;

    let self_promotion = Call::patch(format!("/api/orgs/acme/members/{}", bob.user))
        .with_session(&bob.session)
        .json(serde_json::json!({ "role": "owner" }))
        .send(&h.router)
        .await;
    self_promotion.expect(StatusCode::FORBIDDEN);

    let demote_the_owner = Call::patch(format!("/api/orgs/acme/members/{}", rob.user))
        .with_session(&bob.session)
        .json(serde_json::json!({ "role": "member" }))
        .send(&h.router)
        .await;
    demote_the_owner.expect(StatusCode::FORBIDDEN);
}

/// Deletion is strictly stronger than demotion: an admin blocked from
/// demoting an owner (above) must not reach the same outcome by removing
/// them outright. A second owner keeps the last-owner guard from being the
/// thing that blocks the request, isolating the privilege check.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_admin_cannot_remove_an_owner(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let carol = onboard(&h, "carol@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Owner).await;
    add_member(&h, org, carol.user, Role::Admin).await;

    let remove_owner = Call::delete(format!("/api/orgs/acme/members/{}", bob.user))
        .with_session(&carol.session)
        .send(&h.router)
        .await;
    remove_owner.expect(StatusCode::FORBIDDEN);

    // An owner may still remove another owner.
    Call::delete(format!("/api/orgs/acme/members/{}", bob.user))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);
}

/// Removing someone has to disconnect their agents too. A token that outlives
/// the membership it was granted under is the interesting failure here — the
/// console would show them gone while their agent kept working the queue.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn removing_a_member_revokes_their_tokens_for_that_org(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;

    let acme = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, acme, bob.user, Role::Member).await;
    // Bob is also in an unrelated org, which must be untouched.
    org_with_owner(&h, "globex", &bob).await;

    let minted = Call::post("/api/orgs/acme/tokens")
        .with_session(&bob.session)
        .json(serde_json::json!({ "name": "bob's laptop" }))
        .send(&h.router)
        .await;
    minted.expect(StatusCode::CREATED);
    let token = minted.body["token"].as_str().unwrap().to_string();

    let elsewhere = Call::post("/api/orgs/globex/tokens")
        .with_session(&bob.session)
        .json(serde_json::json!({ "name": "same laptop, other org" }))
        .send(&h.router)
        .await;
    elsewhere.expect(StatusCode::CREATED);
    let other_token = elsewhere.body["token"].as_str().unwrap().to_string();

    // The token works before removal.
    df_auth::tokens::introspect(&h.db, &token, common::RESOURCE)
        .await
        .expect("a freshly minted token should introspect");

    Call::delete(format!("/api/orgs/acme/members/{}", bob.user))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    assert!(
        df_auth::tokens::introspect(&h.db, &token, common::RESOURCE)
            .await
            .is_err(),
        "a removed member's agent kept a working token"
    );
    df_auth::tokens::introspect(&h.db, &other_token, common::RESOURCE)
        .await
        .expect("their token for an unrelated org must survive");

    // And their session is untouched: it is how they reach that other org.
    Call::get("/api/me")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

// ---------------------------------------------------------------- invites

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_invitation_is_mailed_accepted_once_and_grants_its_role(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let bob = onboard(&h, "bob@acme.test").await;
    h.mailer.clear();

    let invited = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test", "role": "admin" }))
        .send(&h.router)
        .await;
    invited.expect(StatusCode::CREATED);

    let mail = h.mailer.last().expect("no invitation mail");
    assert_eq!(mail.to, "bob@acme.test");
    assert!(
        mail.subject.contains("acme"),
        "the org must be named: {}",
        mail.subject
    );
    let token = link_token(&mail);

    let pending = Call::get("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    pending.expect(StatusCode::OK);
    assert_eq!(pending.body.as_array().unwrap().len(), 1);

    let joined = Call::post("/api/orgs/acme/invites/accept")
        .with_session(&bob.session)
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await;
    joined.expect(StatusCode::OK);
    assert_eq!(joined.body["role"], "admin");

    // Bob can now do admin things, and the invitation is spent.
    Call::post("/api/orgs/acme/teams")
        .with_session(&bob.session)
        .json(serde_json::json!({ "slug": "platform" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let replayed = Call::post("/api/orgs/acme/invites/accept")
        .with_session(&bob.session)
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await;
    replayed.expect(StatusCode::GONE);
}

/// A forwarded invitation mail must not be a way into someone else's org.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_invitation_cannot_be_accepted_by_the_wrong_account(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let mallory = onboard(&h, "mallory@evil.test").await;
    h.mailer.clear();

    Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let token = link_token(&h.mailer.last().unwrap());
    let refused = Call::post("/api/orgs/acme/invites/accept")
        .with_session(&mallory.session)
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::FORBIDDEN);
    assert_eq!(refused.error_code(), Some("invite_wrong_account"));
    assert!(
        h.db.member_role(
            h.db.get_org_by_slug("acme").await.unwrap().unwrap().id,
            mallory.user
        )
        .await
        .unwrap()
        .is_none(),
        "the wrong account was admitted"
    );

    // And the invitation is still there for the right person.
    let bob = onboard(&h, "bob@acme.test").await;
    Call::post("/api/orgs/acme/invites/accept")
        .with_session(&bob.session)
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_withdrawn_invitation_stops_working(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let bob = onboard(&h, "bob@acme.test").await;
    h.mailer.clear();

    let invited = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test" }))
        .send(&h.router)
        .await;
    invited.expect(StatusCode::CREATED);
    let id = invited.body["id"].as_str().unwrap().to_string();
    let token = link_token(&h.mailer.last().unwrap());

    Call::delete(format!("/api/orgs/acme/invites/{id}"))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    Call::post("/api/orgs/acme/invites/accept")
        .with_session(&bob.session)
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await
        .expect(StatusCode::GONE);
}

/// An invitation whose mail never went out must not be left live. Nobody has
/// the link, it is invisible to the admin as a problem, and the "one live
/// invite per address" rule would make a retry supersede it anyway.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_invitation_whose_mail_fails_is_withdrawn(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    h.mailer.start_failing();

    let attempted = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test" }))
        .send(&h.router)
        .await;

    attempted.expect(StatusCode::BAD_GATEWAY);
    assert_eq!(attempted.error_code(), Some("mail_undeliverable"));
    assert!(
        !attempted.text.contains("told this mailer to fail"),
        "the provider's own message is for the operator, not the caller: {}",
        attempted.text
    );

    let pending = Call::get("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    pending.expect(StatusCode::OK);
    assert!(
        pending.body.as_array().unwrap().is_empty(),
        "an invitation nobody received was left live: {}",
        pending.text
    );
}

/// Only an owner may hand out ownership, whether directly or by invitation —
/// otherwise the invite endpoint is a way around the role check on members.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_admin_cannot_invite_an_owner(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Admin).await;

    let refused = Call::post("/api/orgs/acme/invites")
        .with_session(&bob.session)
        .json(serde_json::json!({ "email": "eve@acme.test", "role": "owner" }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::FORBIDDEN);

    // The same admin may invite an ordinary member.
    Call::post("/api/orgs/acme/invites")
        .with_session(&bob.session)
        .json(serde_json::json!({ "email": "eve@acme.test", "role": "member" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
}

// ------------------------------------------------------------------ teams

#[sqlx::test(migrations = "../df-core/migrations")]
async fn teams_scope_repos_and_refuse_to_widen_them_on_delete(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let team = Call::post("/api/orgs/acme/teams")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "Platform", "name": "Platform Engineering" }))
        .send(&h.router)
        .await;
    team.expect(StatusCode::CREATED);
    assert_eq!(team.body["slug"], "platform", "slugs are lowercased");
    let team_id = team.body["id"].as_str().unwrap().to_string();

    Call::put(format!(
        "/api/orgs/acme/teams/platform/members/{}",
        rob.user
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await
    .expect(StatusCode::NO_CONTENT);

    let roster = Call::get("/api/orgs/acme/teams/platform/members")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    roster.expect(StatusCode::OK);
    assert_eq!(roster.body.as_array().unwrap().len(), 1);

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api", "teamId": team_id }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let refused = Call::delete("/api/orgs/acme/teams/platform")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    refused.expect(StatusCode::CONFLICT);
    assert_eq!(refused.error_code(), Some("team_in_use"));
    assert!(
        refused.text.contains("api"),
        "the refusal should name the repo"
    );

    // Unassign the repo, and the delete goes through.
    Call::patch("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .json(serde_json::json!({ "teamId": null }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    Call::delete("/api/orgs/acme/teams/platform")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_team_slug_names_the_alternatives(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    Call::post("/api/orgs/acme/teams")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "platform" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    let missed = Call::get("/api/orgs/acme/teams/platfrom")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    missed.expect(StatusCode::NOT_FOUND);
    assert!(
        missed.text.contains("platform"),
        "an error that does not name the alternatives makes a caller guess: {}",
        missed.text
    );
}

// ------------------------------------------------------------------ queue

/// Enqueue through `df-core`, because the console cannot.
///
/// That asymmetry is the point of the queue routes: a job is created and
/// completed by the agent doing the work, over MCP. Reaching past the API to
/// set up this fixture is not a shortcut around a route that exists — it is the
/// only way to reach the state, and a test that could enqueue over HTTP would
/// be evidence of a route that should not be there.
async fn enqueue(
    h: &common::Harness,
    org: df_core::ids::OrgId,
    repo: df_core::ids::RepoId,
    title: &str,
    created_by: df_core::ids::UserId,
) -> df_core::jobs::Job {
    let mut tx = h.db.begin(org).await.unwrap();
    let job = tx
        .add_job(df_core::jobs::NewJob {
            repo_id: repo,
            title: title.into(),
            created_by: Some(created_by),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    job
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_queue_view_lists_filters_and_counts(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let sam = onboard(&h, "sam@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, acme, sam.user, Role::Member).await;

    let api: df_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "api" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };
    let web: df_core::ids::RepoId = {
        let created = Call::post("/api/orgs/acme/repos")
            .with_session(&rob.session)
            .json(serde_json::json!({ "slug": "web" }))
            .send(&h.router)
            .await;
        created.expect(StatusCode::CREATED);
        created.body["id"].as_str().unwrap().parse().unwrap()
    };

    let first = enqueue(&h, acme, api, "rewrite the resolver", rob.user).await;
    enqueue(&h, acme, api, "add a lease test", rob.user).await;
    enqueue(&h, acme, web, "fix the meter", sam.user).await;

    // Claiming one moves it off `pending`, which is what makes the status
    // filter and the counters worth asserting separately.
    {
        let mut tx = h.db.begin(acme).await.unwrap();
        tx.claim_jobs(
            std::slice::from_ref(&first.id),
            rob.user,
            Some("claude-code"),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let all = Call::get("/api/orgs/acme/jobs")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    all.expect(StatusCode::OK);
    assert_eq!(
        all.body.as_array().unwrap().len(),
        3,
        "any member sees the whole org's queue"
    );

    let pending = Call::get("/api/orgs/acme/jobs?status=pending")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    pending.expect(StatusCode::OK);
    assert_eq!(pending.body.as_array().unwrap().len(), 2);

    let in_repo = Call::get("/api/orgs/acme/jobs?repo=web")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    in_repo.expect(StatusCode::OK);
    assert_eq!(in_repo.body.as_array().unwrap().len(), 1);
    assert_eq!(in_repo.body[0]["title"], "fix the meter");

    let mine = Call::get("/api/orgs/acme/jobs?mine=true")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    mine.expect(StatusCode::OK);
    assert_eq!(
        mine.body.as_array().unwrap().len(),
        1,
        "`mine` is the caller, not the org"
    );

    let stats = Call::get("/api/orgs/acme/jobs/stats")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    stats.expect(StatusCode::OK);
    assert_eq!(stats.body["total"], 3);
    assert_eq!(stats.body["pending"], 2);
    assert_eq!(stats.body["inProgress"], 1);
    assert_eq!(stats.body["blocked"], 0);

    // `/jobs/stats` and `/jobs/{job}` share a prefix; a router that resolved
    // the literal to the parameter would answer this with a 404 for a job
    // called "stats".
    let one = Call::get(format!("/api/orgs/acme/jobs/{}", first.id))
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    one.expect(StatusCode::OK);
    assert_eq!(one.body["title"], "rewrite the resolver");
    assert_eq!(one.body["status"], "in-progress");
    assert_eq!(one.body["claimedByLabel"], "claude-code");
    assert_eq!(one.body["dependsOn"].as_array().unwrap().len(), 0);

    let repo_stats = Call::get("/api/orgs/acme/jobs/stats?repo=api")
        .with_session(&sam.session)
        .send(&h.router)
        .await;
    repo_stats.expect(StatusCode::OK);
    assert_eq!(repo_stats.body["total"], 2);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_job_detail_names_what_it_is_waiting_for(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    let api: df_core::ids::RepoId = created.body["id"].as_str().unwrap().parse().unwrap();

    let blocker = enqueue(&h, acme, api, "land the migration", rob.user).await;
    let blocked = {
        let mut tx = h.db.begin(acme).await.unwrap();
        let job = tx
            .add_job(df_core::jobs::NewJob {
                repo_id: api,
                title: "use the new column".into(),
                depends_on: vec![blocker.id.clone()],
                created_by: Some(rob.user),
                ..Default::default()
            })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        job
    };

    let detail = Call::get(format!("/api/orgs/acme/jobs/{}", blocked.id))
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    detail.expect(StatusCode::OK);
    assert_eq!(detail.body["dependsOn"][0], blocker.id.as_str());

    // A blocked job is still pending. The overview counts it in both, and the
    // difference is the whole reason `blocked` is reported at all: two pending
    // jobs where one cannot start is not the same queue as two that can.
    let stats = Call::get("/api/orgs/acme/jobs/stats")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    stats.expect(StatusCode::OK);
    assert_eq!(stats.body["pending"], 2);
    assert_eq!(stats.body["blocked"], 1);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_repo_filter_names_the_registered_slugs(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // Not an empty list. A filter that silently matched nothing would render a
    // queue that looks quiet rather than one that was never asked about.
    let missed = Call::get("/api/orgs/acme/jobs?repo=apo")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    missed.expect(StatusCode::NOT_FOUND);
    assert!(
        missed.text.contains("api"),
        "the error should name what is registered: {}",
        missed.text
    );

    let bad_status = Call::get("/api/orgs/acme/jobs?status=done")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    bad_status.expect(StatusCode::BAD_REQUEST);
    assert!(
        bad_status.text.contains("completed"),
        "an unknown status should list the valid ones: {}",
        bad_status.text
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn one_orgs_queue_is_invisible_to_another(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let mal = onboard(&h, "mal@other.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "other", &mal).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    let api: df_core::ids::RepoId = created.body["id"].as_str().unwrap().parse().unwrap();
    let job = enqueue(&h, acme, api, "acme's secret roadmap", rob.user).await;

    // 404, not 403: a 403 on a real slug and a 404 on a fake one turns any
    // signed-in account into a directory of who uses the product.
    for uri in [
        "/api/orgs/acme/jobs".to_string(),
        "/api/orgs/acme/jobs/stats".to_string(),
        format!("/api/orgs/acme/jobs/{}", job.id),
    ] {
        let refused = Call::get(&uri)
            .with_session(&mal.session)
            .send(&h.router)
            .await;
        refused.expect(StatusCode::NOT_FOUND);
        assert!(
            !refused.text.contains("secret roadmap"),
            "{uri} leaked another org's job"
        );
    }

    // And the same job id, asked for from an org that has one of its own, is
    // that org's job or nothing — ids are per-org counters, so `job-1` exists
    // in both and must not cross.
    let elsewhere = Call::get("/api/orgs/other/jobs/job-1")
        .with_session(&mal.session)
        .send(&h.router)
        .await;
    elsewhere.expect(StatusCode::NOT_FOUND);
}

// --------------------------------------------------------- tokens & usage

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_pat_is_shown_once_audienced_for_mcp_and_revocable(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let minted = Call::post("/api/orgs/acme/tokens")
        .with_session(&rob.session)
        .json(serde_json::json!({ "name": "laptop", "scopes": ["jobs:read", "jobs:write"] }))
        .send(&h.router)
        .await;
    minted.expect(StatusCode::CREATED);

    let token = minted.body["token"].as_str().unwrap().to_string();
    let id = minted.body["id"].as_str().unwrap().to_string();
    assert!(token.starts_with("df_pat_"));
    assert_eq!(minted.body["resource"], common::RESOURCE);

    let principal = df_auth::tokens::introspect(&h.db, &token, common::RESOURCE)
        .await
        .expect("a minted PAT must introspect against the MCP resource");
    assert!(principal.has_scope("jobs:write"));

    // Audienced: a token for this resource is refused by any other.
    assert!(
        df_auth::tokens::introspect(&h.db, &token, "https://someone-else.test/mcp")
            .await
            .is_err(),
        "the audience check is what stops a confused deputy"
    );

    // Listed without the secret.
    let listed = Call::get("/api/orgs/acme/tokens")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    assert!(
        !listed.text.contains(&token),
        "the token itself must never be listed back"
    );

    Call::delete(format!("/api/orgs/acme/tokens/{id}"))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    assert!(
        df_auth::tokens::introspect(&h.db, &token, common::RESOURCE)
            .await
            .is_err(),
        "revocation must take effect immediately"
    );
}

/// A PAT must not be a way to obtain a scope the console would not grant.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_member_cannot_mint_an_admin_scoped_token(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Member).await;

    let refused = Call::post("/api/orgs/acme/tokens")
        .with_session(&bob.session)
        .json(serde_json::json!({ "name": "sneaky", "scopes": ["org:admin"] }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::FORBIDDEN);

    let unknown_scope = Call::post("/api/orgs/acme/tokens")
        .with_session(&bob.session)
        .json(serde_json::json!({ "name": "typo", "scopes": ["jobs:destroy"] }))
        .send(&h.router)
        .await;
    unknown_scope.expect(StatusCode::BAD_REQUEST);
    assert!(
        unknown_scope.text.contains("jobs:read"),
        "an unknown scope should list the supported ones: {}",
        unknown_scope.text
    );
}

/// You cannot revoke another member's token even holding its id.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn one_member_cannot_revoke_anothers_token(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Member).await;

    let minted = Call::post("/api/orgs/acme/tokens")
        .with_session(&bob.session)
        .json(serde_json::json!({ "name": "bob's" }))
        .send(&h.router)
        .await;
    minted.expect(StatusCode::CREATED);
    let id = minted.body["id"].as_str().unwrap().to_string();
    let token = minted.body["token"].as_str().unwrap().to_string();

    // Even the owner cannot reach into someone else's credential from here.
    Call::delete(format!("/api/orgs/acme/tokens/{id}"))
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NOT_FOUND);

    df_auth::tokens::introspect(&h.db, &token, common::RESOURCE)
        .await
        .expect("the token should still be live");
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn usage_reads_the_same_numbers_the_agent_sees(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;

    // Meter something the way a tool call would.
    let mut tx = h.db.begin(org).await.unwrap();
    tx.record_usage(Some(rob.user), "add_job", true)
        .await
        .unwrap();
    tx.record_usage(Some(rob.user), "watch", false)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let usage = Call::get("/api/orgs/acme/usage")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    usage.expect(StatusCode::OK);

    assert_eq!(usage.body["plan"], "Free");
    assert_eq!(usage.body["billableUsed"], 1, "only billable calls count");
    assert_eq!(usage.body["totalCalls"], 2, "every call is recorded");
    assert_eq!(
        usage.body["enforced"], false,
        "enforcement is off by default"
    );
}

// ---------------------------------------------------------------- openapi

/// The document is served, describes the routes that exist, and needs no
/// credential — a client generator cannot log in.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_openapi_document_is_public_and_describes_the_surface(pool: PgPool) {
    let h = harness(pool);
    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);

    assert_eq!(doc.body["openapi"], "3.1.0");
    assert!(doc.body["paths"]["/api/orgs/{org}/repos"]["post"].is_object());
    assert_eq!(
        doc.body["components"]["securitySchemes"]["sessionCookie"]["name"],
        "__Host-df_session"
    );
}

/// Routes are mounted from the same list the document is rendered from, so
/// anything described has to actually answer. This catches the failure the
/// catalog exists to prevent — a documented endpoint that is not mounted.
///
/// A handler's own `404` ("no such repo") is a pass; the router's is not. They
/// are told apart by the body: every failure this crate produces carries the
/// `{"error": {...}}` envelope, and a route that does not exist produces an
/// empty one.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn every_documented_get_is_actually_mounted(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let doc = Call::get("/api/openapi.json").send(&h.router).await;
    let paths = doc.body["paths"].as_object().unwrap().clone();
    let mut checked = 0;

    for (path, operations) in paths {
        if operations.get("get").is_none() {
            continue;
        }
        let concrete = path
            .replace("{org}", "acme")
            .replace("{team}", "platform")
            .replace("{repo}", "api")
            .replace("{user}", &rob.user.to_string())
            .replace("{id}", &uuid::Uuid::nil().to_string());

        let reply = Call::get(&concrete)
            .with_session(&rob.session)
            .send(&h.router)
            .await;
        checked += 1;

        assert_ne!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "GET {concrete} is documented but the path serves other methods only"
        );
        if reply.status == StatusCode::NOT_FOUND {
            assert!(
                reply.error_code().is_some(),
                "GET {concrete} is documented but not mounted — the 404 came from \
                 the router, not from a handler (body was {:?})",
                reply.text
            );
        }
    }

    assert!(
        checked > 10,
        "only {checked} GETs were checked; the document looks empty"
    );
}
