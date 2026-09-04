//! The console API, end to end against a real Postgres.
//!
//! These drive the assembled router, so a test failing here means a real client
//! would have failed the same way. The properties under test are the ones a
//! unit test cannot reach: that the onboarding sequence actually completes, that
//! an org you are not in is indistinguishable from one that does not exist, that
//! a role boundary holds at the HTTP edge, and that removing someone actually
//! disconnects their agents.

mod common;

use common::{add_member, harness, harness_with_trackers, onboard, org_with_owner, sign_in, Call};
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

    let me = Call::get("/api/me")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    me.expect(StatusCode::OK);
    assert_eq!(me.body["user"]["email"], "rob@acme.test");
    assert_eq!(me.body["passkeyCount"], 1);
    assert_eq!(
        me.body["shouldAddPasskey"], true,
        "one passkey is one device — the console has to ask for a second"
    );
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
#[sqlx::test(migrations = "../df-core/migrations")]
async fn signing_in_and_out_works_and_a_dead_cookie_is_refused(pool: PgPool) {
    let h = harness(pool);
    let mut rob = onboard(&h, "rob@acme.test").await;

    let signed_in = sign_in(&h, &mut rob).await;
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
#[sqlx::test(migrations = "../df-core/migrations")]
async fn signing_out_everywhere_ends_every_session(pool: PgPool) {
    let h = harness(pool);
    let mut rob = onboard(&h, "rob@acme.test").await;

    let second = sign_in(&h, &mut rob).await.session_cookie().unwrap();

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
async fn an_invitation_code_is_handed_back_accepted_once_and_grants_its_role(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let bob = onboard(&h, "bob@acme.test").await;

    let invited = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test", "role": "admin" }))
        .send(&h.router)
        .await;
    invited.expect(StatusCode::CREATED);

    // The code comes back to the admin — there is no mailbox it went to
    // instead — and the link is the same secret wrapped in a console URL.
    let token = invited.body["code"].as_str().expect("no code").to_string();
    assert!(token.starts_with("df_inv_"), "unexpected code: {token}");
    assert_eq!(
        invited.body["link"],
        format!("https://console.dark-factory.test/invite/acme?token={token}")
    );

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

    let invited = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test" }))
        .send(&h.router)
        .await;
    invited.expect(StatusCode::CREATED);

    let token = invited.body["code"].as_str().unwrap().to_string();
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

    let invited = Call::post("/api/orgs/acme/invites")
        .with_session(&rob.session)
        .json(serde_json::json!({ "email": "bob@acme.test" }))
        .send(&h.router)
        .await;
    invited.expect(StatusCode::CREATED);
    let id = invited.body["id"].as_str().unwrap().to_string();
    let token = invited.body["code"].as_str().unwrap().to_string();

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

/// The only assisted way back into an account, and the limits that make it
/// safe to have. Without email there is nothing else, so this endpoint carries
/// weight the mailed recovery link used to.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_admin_can_reset_a_members_authenticator_but_gains_nothing_by_it(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let mut bob = onboard(&h, "bob@acme.test").await;
    add_member(
        &h,
        h.db.get_org_by_slug("acme").await.unwrap().unwrap().id,
        bob.user,
        df_core::orgs::Role::Member,
    )
    .await;

    let reset = Call::post(format!(
        "/api/orgs/acme/members/{}/reset-passkeys",
        bob.user
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await;
    reset.expect(StatusCode::CREATED);

    // Bob's old passkey is gone...
    let stale = sign_in(&h, &mut bob).await;
    assert_ne!(stale.status, StatusCode::OK, "the old passkey still works");

    // ...and so is his session, so a reset actually interrupts whoever holds
    // the account rather than leaving them running.
    let dead = Call::get("/api/me")
        .with_session(&bob.session)
        .send(&h.router)
        .await;
    assert_eq!(dead.status, StatusCode::UNAUTHORIZED);

    // The admin gained nothing directly: no session was handed to Rob for Bob.
    // What Rob holds is the claim code, and it is the *only* way back — an
    // account with no passkeys is otherwise claimable by whoever reaches
    // registration first, which is the takeover this endpoint exists to close.
    let code = reset.body["code"]
        .as_str()
        .expect("no claim code")
        .to_string();

    let mut new_device = common::authenticator();
    let started = Call::post("/api/auth/claim/start")
        .json(serde_json::json!({ "code": code }))
        .send(&h.router)
        .await;
    started.expect(StatusCode::OK);

    let reclaimed = common::finish_registration(
        &h,
        &mut new_device,
        "/api/auth/claim/finish",
        &started.body,
        serde_json::json!({ "code": code }),
    )
    .await;
    reclaimed.expect(StatusCode::OK);
    assert_eq!(
        reclaimed.body["user"]["id"].as_str().unwrap(),
        bob.user.to_string(),
        "the claim must land on the account it was issued for"
    );

    // And the code is spent.
    let replayed = Call::post("/api/auth/claim/start")
        .json(serde_json::json!({ "code": code }))
        .send(&h.router)
        .await;
    assert_ne!(replayed.status, StatusCode::OK, "a claim code was reusable");
}

/// Without a claim code, a reset account must not be claimable at all — that
/// race is the takeover the code exists to prevent.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_reset_account_cannot_be_claimed_without_the_code(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    let bob = onboard(&h, "bob@acme.test").await;
    add_member(&h, org, bob.user, df_core::orgs::Role::Member).await;

    Call::post(format!(
        "/api/orgs/acme/members/{}/reset-passkeys",
        bob.user
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await
    .expect(StatusCode::CREATED);

    // A stranger with a guessed code gets nowhere.
    let guessed = Call::post("/api/auth/claim/start")
        .json(serde_json::json!({ "code": "df_inv_not-a-real-code" }))
        .send(&h.router)
        .await;
    assert_ne!(guessed.status, StatusCode::OK);

    // And signing up creates a *new* account rather than claiming Bob's — there
    // is no identifier to aim at, which is the whole point of the passkey-first
    // signup. Bob's account keeps its membership and its address.
    let stranger = onboard(&h, "stranger@acme.test").await;
    assert_ne!(stranger.user, bob.user);
    assert!(
        h.db.member_role(org, stranger.user)
            .await
            .unwrap()
            .is_none(),
        "a stranger's new account inherited a reset member's org"
    );
}

/// An admin must not reach through this endpoint what the role check refuses
/// everywhere else — resetting an owner is an owner's business.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_admin_cannot_reset_an_owners_authenticator(pool: PgPool) {
    let h = harness(pool);
    let mut owner = onboard(&h, "owner@acme.test").await;
    let org = org_with_owner(&h, "acme", &owner).await;
    let admin = onboard(&h, "admin@acme.test").await;
    add_member(&h, org, admin.user, df_core::orgs::Role::Admin).await;

    let refused = Call::post(format!(
        "/api/orgs/acme/members/{}/reset-passkeys",
        owner.user
    ))
    .with_session(&admin.session)
    .send(&h.router)
    .await;
    refused.expect(StatusCode::FORBIDDEN);

    // The owner's credential is untouched.
    sign_in(&h, &mut owner).await.expect(StatusCode::OK);
}

// --------------------------------------------------------------- trackers

/// Tracker setup grants a repo the ability to move a customer's tickets, so
/// every route that writes one is admin-only. The read is a member read for the
/// same reason `GET /repos` is: it describes a repo the member can already see.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn only_an_admin_can_connect_a_tracker_or_bind_a_repo(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;

    let org = org_with_owner(&h, "acme", &rob).await;
    add_member(&h, org, bob.user, Role::Member).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    // A member reads a repo's bindings.
    Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&bob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    for call in [
        Call::get("/api/orgs/acme/tracker-connections"),
        Call::post("/api/orgs/acme/tracker-connections/github")
            .json(serde_json::json!({ "code": "x", "installationId": 17 })),
        Call::delete("/api/orgs/acme/tracker-connections/github"),
        Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
            .json(serde_json::json!({ "externalRef": "acme/api" })),
        Call::delete("/api/orgs/acme/repos/api/tracker-bindings/github"),
    ] {
        call.with_session(&bob.session)
            .send(&h.router)
            .await
            .expect(StatusCode::FORBIDDEN);
    }
}

/// The same "an org you are not in is a 404, never a 403" rule the rest of the
/// console keeps. A tracker route is a particularly bad place to break it: the
/// binding names the customer's JIRA project key.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn another_orgs_tracker_routes_are_invisible(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let mallory = onboard(&h, "mallory@evil.test").await;

    org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "evil", &mallory).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    for (real, imaginary) in [
        (
            Call::get("/api/orgs/acme/tracker-connections"),
            Call::get("/api/orgs/no-such-org/tracker-connections"),
        ),
        (
            Call::delete("/api/orgs/acme/tracker-connections/github"),
            Call::delete("/api/orgs/no-such-org/tracker-connections/github"),
        ),
        (
            Call::get("/api/orgs/acme/repos/api/tracker-bindings"),
            Call::get("/api/orgs/no-such-org/repos/api/tracker-bindings"),
        ),
        (
            Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
                .json(serde_json::json!({ "externalRef": "acme/api" })),
            Call::put("/api/orgs/no-such-org/repos/api/tracker-bindings/github")
                .json(serde_json::json!({ "externalRef": "acme/api" })),
        ),
    ] {
        let real = real.with_session(&mallory.session).send(&h.router).await;
        let imaginary = imaginary
            .with_session(&mallory.session)
            .send(&h.router)
            .await;

        real.expect(StatusCode::NOT_FOUND);
        assert_eq!(
            real.status, imaginary.status,
            "a real org you are not in must answer like one that does not exist"
        );
        assert_eq!(real.error_code(), imaginary.error_code());
    }
}

/// A binding that can never match an inbound event is a configuration error
/// worth catching where it is typed, not at 3am when the label appears to do
/// nothing. `owner/repo` is what webhook ingest matches `repository.full_name`
/// against; a project key is what it matches `fields.project.key` against.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_binding_that_could_never_match_an_event_is_refused(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({ "slug": "api" }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);

    for (provider, bad) in [
        ("github", "acme"),
        ("github", "acme/api/extra"),
        ("github", "acme/"),
        ("jira", "not a key"),
        ("jira", "acme-123"),
    ] {
        let refused = Call::put(format!(
            "/api/orgs/acme/repos/api/tracker-bindings/{provider}"
        ))
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": bad }))
        .send(&h.router)
        .await;
        refused.expect(StatusCode::BAD_REQUEST);
        assert!(
            !refused.text.is_empty(),
            "{provider} binding {bad:?} was refused with no explanation"
        );
    }

    // And the shapes that can match are accepted, and round-trip.
    Call::put("/api/orgs/acme/repos/api/tracker-bindings/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": "acme/api", "triggerLabel": "agent" }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
    Call::put("/api/orgs/acme/repos/api/tracker-bindings/jira")
        .with_session(&rob.session)
        .json(serde_json::json!({ "externalRef": "ACME" }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let listed = Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(listed.text.contains("acme/api"), "{}", listed.text);
    assert!(listed.text.contains("ACME"), "{}", listed.text);
    assert!(
        listed.text.contains("agent"),
        "the trigger label is what inbound sync watches for: {}",
        listed.text
    );

    Call::delete("/api/orgs/acme/repos/api/tracker-bindings/jira")
        .with_session(&rob.session)
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    let after = Call::get("/api/orgs/acme/repos/api/tracker-bindings")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    assert!(!after.text.contains("ACME"), "{}", after.text);
}

/// Ciphertext is not a secret in the sense that leaking it grants access, but a
/// console `GET` handing every admin's browser the sealed JIRA refresh token is
/// gratuitous exposure of exactly the material `DF_ENCRYPTION_KEY` exists to
/// protect. The listing is a view type for this reason, not the domain row.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_connection_listing_never_carries_stored_secrets(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;

    let mut tx = h.db.begin(org).await.expect("tx");
    df_core::trackers::upsert_connection(
        &mut tx,
        df_core::trackers::Provider::Jira,
        "site-1",
        Some(&common::cipher().seal(b"jira-refresh-token").expect("seal")),
        None,
    )
    .await
    .expect("connection");
    tx.commit().await.expect("commit");

    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);

    assert!(
        listed.text.contains("site-1"),
        "the external id is what the admin needs to see: {}",
        listed.text
    );
    assert!(
        !listed.text.contains("encrypted"),
        "the console listing carried stored ciphertext: {}",
        listed.text
    );
    assert!(
        listed.text.contains("hasCredentials"),
        "the page still needs to know a credential is stored: {}",
        listed.text
    );
}

/// A deployment with no GitHub OAuth client cannot finish a connect flow, and
/// says so instead of walking an admin through an install they then have to
/// undo by hand. The test harness configures no provider, which is exactly the
/// deployment shape being asserted.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_deployment_that_cannot_connect_a_provider_says_so(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(
        listed.text.contains("\"configured\":false"),
        "an unconfigured deployment must not advertise a connect flow: {}",
        listed.text
    );

    let refused = Call::post("/api/orgs/acme/tracker-connections/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "code": "x", "installationId": 17 }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("not configured"),
        "the refusal should name the deployment's gap: {}",
        refused.text
    );
}

/// GitHub's connect flow needs an installation id and JIRA's does not have one.
/// A GitHub request without it is refused rather than defaulted, because the
/// value is what the whole verification is about — and refused *before* any
/// call to GitHub, which is what lets this test run without a network.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn connecting_github_without_an_installation_id_is_refused(pool: PgPool) {
    let h = harness_with_trackers(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    // A configured deployment advertises both flows, with the provider URLs
    // built server-side so no App slug or client id is baked into the bundle.
    let listed = Call::get("/api/orgs/acme/tracker-connections")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    listed.expect(StatusCode::OK);
    assert!(
        listed
            .text
            .contains("github.com/apps/dark-factory/installations/new"),
        "the install link is built from the configured slug: {}",
        listed.text
    );
    assert!(
        listed.text.contains("offline_access"),
        "without offline_access JIRA returns no refresh token and the connection dies \
         an hour after it is made: {}",
        listed.text
    );

    let refused = Call::post("/api/orgs/acme/tracker-connections/github")
        .with_session(&rob.session)
        .json(serde_json::json!({ "code": "x" }))
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("installationId"),
        "the refusal should name the missing field: {}",
        refused.text
    );
}

/// An unknown provider names the two that exist rather than 404ing into
/// silence — the same rule the MCP errors follow.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unknown_provider_names_the_ones_that_exist(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let refused = Call::delete("/api/orgs/acme/tracker-connections/linear")
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert!(
        refused.text.contains("github") && refused.text.contains("jira"),
        "the refusal should list the valid providers: {}",
        refused.text
    );
}

/// The console API no longer writes the free-form `repos.tracker_binding` blob
/// — `tracker_bindings` rows replaced it, and they are what webhook ingest and
/// the sync engine actually read.
///
/// **Dropping the field must not turn it into a `400`.** An unknown field is
/// not an error in this API, and making it one for this field alone would be a
/// second breaking change on top of the first: a client still sending it would
/// go from "stored somewhere nothing reads" to "cannot register a repo at all".
/// It is ignored, and the repo is created.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_console_ignores_the_retired_tracker_binding_field(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let created = Call::post("/api/orgs/acme/repos")
        .with_session(&rob.session)
        .json(serde_json::json!({
            "slug": "api",
            "trackerBinding": { "jira": "ACME" }
        }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    assert!(
        !created.text.contains("ACME"),
        "the retired field was stored rather than ignored: {}",
        created.text
    );

    let patched = Call::patch("/api/orgs/acme/repos/api")
        .with_session(&rob.session)
        .json(serde_json::json!({
            "name": "API",
            "trackerBinding": { "jira": "ACME" }
        }))
        .send(&h.router)
        .await;
    patched.expect(StatusCode::OK);
    assert!(
        patched.text.contains("API") && !patched.text.contains("ACME"),
        "the update applied the wrong half: {}",
        patched.text
    );
}
