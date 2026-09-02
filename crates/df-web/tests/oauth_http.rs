//! The authorization server over HTTP, driven the way a coding agent drives it.
//!
//! `df-auth`'s own tests cover the protocol logic — PKCE, redirect matching,
//! rotation, audiences. These cover the transport around it, which is where an
//! agent actually fails: whether the discovery document says the right thing,
//! whether the consent form round-trips, whether an error comes back as a page
//! or as a redirect, and whether the token endpoint speaks form-encoded RFC 6749
//! rather than the JSON a modern instinct would reach for.

mod common;

use base64::Engine;
use common::{harness, onboard, org_with_owner, Call, Harness, RESOURCE};
use http::StatusCode;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const REDIRECT: &str = "http://127.0.0.1:1455/callback";

fn pkce() -> (String, String) {
    let verifier = "x".repeat(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Register a client the way an agent does, through the open endpoint.
async fn register(h: &Harness, name: &str, redirect: &str) -> String {
    let registered = Call::post("/oauth/register")
        .json(serde_json::json!({
            "client_name": name,
            "redirect_uris": [redirect],
        }))
        .send(&h.router)
        .await;
    registered.expect(StatusCode::CREATED);
    registered.body["client_id"].as_str().unwrap().to_string()
}

fn authorize_url(client_id: &str, challenge: &str, scope: &str, state: &str) -> String {
    format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fcallback\
         &code_challenge={challenge}&code_challenge_method=S256\
         &scope={}&state={state}",
        scope.replace(':', "%3A").replace(' ', "%20")
    )
}

fn location(reply: &common::Reply) -> String {
    reply
        .headers
        .get(http::header::LOCATION)
        .unwrap_or_else(|| panic!("no Location header; body was {}", reply.text))
        .to_str()
        .unwrap()
        .to_string()
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| percent_decode(value))
    })
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ------------------------------------------------------------- discovery

/// The document every MCP client believes over anything written elsewhere. If
/// it is wrong, onboarding fails in a way that looks like "the server is broken".
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_discovery_documents_describe_this_server(pool: PgPool) {
    let h = harness(pool);

    let meta = Call::get("/.well-known/oauth-authorization-server")
        .send(&h.router)
        .await;
    meta.expect(StatusCode::OK);

    assert_eq!(meta.body["issuer"], common::PUBLIC_URL);
    assert_eq!(
        meta.body["authorization_endpoint"],
        format!("{}/oauth/authorize", common::PUBLIC_URL)
    );
    assert_eq!(
        meta.body["token_endpoint"],
        format!("{}/oauth/token", common::PUBLIC_URL)
    );
    assert_eq!(
        meta.body["code_challenge_methods_supported"],
        serde_json::json!(["S256"]),
        "advertising anything but S256 would invite a client to use it"
    );
    assert_eq!(meta.body["resource_indicators_supported"], true);

    let resource = Call::get("/.well-known/oauth-protected-resource")
        .send(&h.router)
        .await;
    resource.expect(StatusCode::OK);
    assert_eq!(resource.body["resource"], RESOURCE);
    assert_eq!(
        resource.body["authorization_servers"],
        serde_json::json!([common::PUBLIC_URL])
    );
}

// ------------------------------------------------------------ the flow

/// The whole authorization code flow, as a CLI agent runs it.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_agent_gets_a_token_it_can_use_against_the_mcp_surface(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (verifier, challenge) = pkce();

    // The consent screen.
    let page = Call::get(authorize_url(
        &client_id,
        &challenge,
        "jobs:read jobs:write",
        "opaque-state",
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await;
    page.expect(StatusCode::OK);

    assert!(
        page.text.contains("127.0.0.1"),
        "the consent screen must show where the code will be sent"
    );
    assert!(page
        .text
        .contains("Create, claim, update, and complete jobs"));
    assert!(page.text.contains("name=org_id"));

    let org_id = h.db.get_org_by_slug("acme").await.unwrap().unwrap().id;

    // The decision.
    let granted = Call::post("/oauth/authorize")
        .with_session(&rob.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "jobs:read jobs:write"),
            ("resource", RESOURCE),
            ("state", "opaque-state"),
            ("org_id", &org_id.to_string()),
            ("decision", "allow"),
        ])
        .send(&h.router)
        .await;
    granted.expect(StatusCode::SEE_OTHER);

    let callback = location(&granted);
    assert!(callback.starts_with(REDIRECT), "{callback}");
    assert_eq!(
        query_param(&callback, "state").as_deref(),
        Some("opaque-state"),
        "the client's CSRF value must come back unchanged"
    );
    let code = query_param(&callback, "code").expect("no code in the callback");

    // The exchange. Form-encoded, per RFC 6749 — not JSON.
    let tokens = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", &verifier),
            ("resource", RESOURCE),
        ])
        .send(&h.router)
        .await;
    tokens.expect(StatusCode::OK);

    assert_eq!(tokens.body["token_type"], "Bearer");
    assert_eq!(tokens.body["scope"], "jobs:read jobs:write");
    assert_eq!(
        tokens.headers.get(http::header::CACHE_CONTROL).unwrap(),
        "no-store",
        "a token response must not sit in a shared cache"
    );

    let access = tokens.body["access_token"].as_str().unwrap();
    let principal = df_auth::tokens::introspect(&h.db, access, RESOURCE)
        .await
        .expect("the minted token must work against the MCP resource");
    assert_eq!(principal.org_id, org_id);
    assert_eq!(principal.user_id, rob.user);
    assert!(principal.has_scope("jobs:write"));

    // And a refresh rotates.
    let refresh = tokens.body["refresh_token"].as_str().unwrap().to_string();
    let refreshed = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", RESOURCE),
        ])
        .send(&h.router)
        .await;
    refreshed.expect(StatusCode::OK);
    assert_ne!(
        refreshed.body["refresh_token"], tokens.body["refresh_token"],
        "the refresh token must rotate"
    );

    // Replaying the consumed refresh token is a theft signal, and takes the
    // whole chain with it.
    let replayed = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", RESOURCE),
        ])
        .send(&h.router)
        .await;
    replayed.expect(StatusCode::BAD_REQUEST);
    assert_eq!(replayed.body["error"], "invalid_grant");
}

/// A client that omits `scope` gets `DEFAULT_SCOPES`. The consent page shows
/// exactly that list before the human decides — the token issued on "allow"
/// must carry the same scopes, not the empty list a naive read of the
/// original (scope-less) request would produce. Before this was fixed, the
/// hidden `scope` field round-tripped the request's absence rather than the
/// page's normalized default, so the agent came back with a token that
/// failed every tool call after a consent screen had just promised it access.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_scopeless_request_is_granted_the_scopes_the_consent_page_showed(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (verifier, challenge) = pkce();

    let page = Call::get(format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fcallback\
         &code_challenge={challenge}&code_challenge_method=S256&state=s"
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await;
    page.expect(StatusCode::OK);
    assert!(
        page.text.contains("See the work queue"),
        "the consent page must show the default scopes it is about to grant"
    );

    let org_id = h.db.get_org_by_slug("acme").await.unwrap().unwrap().id;

    // Mirrors what the rendered form actually submits: `scope` present but
    // blank, exactly as `blank_to_none` expects from an unfilled hidden field.
    let granted = Call::post("/oauth/authorize")
        .with_session(&rob.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", ""),
            ("state", "s"),
            ("org_id", &org_id.to_string()),
            ("decision", "allow"),
        ])
        .send(&h.router)
        .await;
    granted.expect(StatusCode::SEE_OTHER);

    let code = query_param(&location(&granted), "code").expect("no code in the callback");
    let tokens = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", &verifier),
            ("resource", RESOURCE),
        ])
        .send(&h.router)
        .await;
    tokens.expect(StatusCode::OK);

    assert_eq!(
        tokens.body["scope"],
        df_auth::oauth::DEFAULT_SCOPES.join(" "),
        "the issued token must carry what the consent page displayed, not an \
         empty scope list"
    );

    let access = tokens.body["access_token"].as_str().unwrap();
    let principal = df_auth::tokens::introspect(&h.db, access, RESOURCE)
        .await
        .expect("the minted token must work against the MCP resource");
    assert!(principal.has_scope("jobs:read"));
}

/// A signed-out visitor has to end up somewhere they can act, not at a 401.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unauthenticated_visitor_is_sent_to_log_in_and_comes_back(pool: PgPool) {
    let h = harness(pool);
    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let bounced = Call::get(authorize_url(&client_id, &challenge, "jobs:read", "s"))
        .send(&h.router)
        .await;

    assert_eq!(bounced.status, StatusCode::SEE_OTHER);
    let target = location(&bounced);
    assert!(
        target.starts_with(&format!("{}/login?next=", common::PUBLIC_URL)),
        "{target}"
    );

    let next = query_param(&target, "next").expect("no next parameter");
    assert!(
        next.starts_with("/oauth/authorize?") && next.contains(&client_id),
        "the flow must resume where it left off: {next}"
    );
}

// --------------------------------------------------------- error routing

/// The open-redirector case. An unregistered redirect URI is the one thing that
/// must never be redirected to — it is precisely what could not be verified.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unregistered_redirect_uri_renders_a_page_and_never_redirects(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;
    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let attacked = Call::get(format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=https%3A%2F%2Fevil.test%2Fsteal\
         &code_challenge={challenge}&code_challenge_method=S256&state=s"
    ))
    .with_session(&rob.session)
    .send(&h.router)
    .await;

    attacked.expect(StatusCode::BAD_REQUEST);
    assert!(
        attacked.headers.get(http::header::LOCATION).is_none(),
        "the server redirected to a URI it could not verify — an open redirector"
    );
    assert!(attacked.text.contains("redirect_uri"), "{}", attacked.text);
    assert!(
        !attacked.text.contains("code="),
        "no code may be issued here"
    );
}

/// Once the destination *is* verified, errors go back to the client so an agent
/// can say something useful instead of hanging on a callback that never comes.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn declining_sends_the_client_an_error_not_a_dead_end(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let org_id = org_with_owner(&h, "acme", &rob).await;
    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let declined = Call::post("/oauth/authorize")
        .with_session(&rob.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "jobs:read"),
            ("state", "opaque-state"),
            ("org_id", &org_id.to_string()),
            ("decision", "deny"),
        ])
        .send(&h.router)
        .await;
    declined.expect(StatusCode::SEE_OTHER);

    let callback = location(&declined);
    assert_eq!(
        query_param(&callback, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(
        query_param(&callback, "state").as_deref(),
        Some("opaque-state")
    );
    assert!(query_param(&callback, "code").is_none());
}

/// The org on a token is decided here and cannot be changed afterwards, so a
/// hand-edited form field is the whole attack surface for a cross-tenant token.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn consent_cannot_name_an_org_the_caller_is_not_in(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let mallory = onboard(&h, "mallory@evil.test").await;
    let acme = org_with_owner(&h, "acme", &rob).await;
    org_with_owner(&h, "evil", &mallory).await;

    let client_id = register(&h, "Mallory's Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let attempted = Call::post("/oauth/authorize")
        .with_session(&mallory.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "jobs:read"),
            ("org_id", &acme.to_string()),
            ("decision", "allow"),
        ])
        .send(&h.router)
        .await;
    attempted.expect(StatusCode::SEE_OTHER);

    let callback = location(&attempted);
    assert_eq!(
        query_param(&callback, "error").as_deref(),
        Some("access_denied"),
        "a token was issued for an org the caller does not belong to"
    );
    assert!(query_param(&callback, "code").is_none());
}

/// A client cannot be granted authority the human granting it does not hold.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_member_cannot_consent_to_org_admin(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let bob = onboard(&h, "bob@acme.test").await;
    let org = org_with_owner(&h, "acme", &rob).await;
    common::add_member(&h, org, bob.user, df_core::orgs::Role::Member).await;

    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let refused = Call::post("/oauth/authorize")
        .with_session(&bob.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "org:admin"),
            ("org_id", &org.to_string()),
            ("decision", "allow"),
        ])
        .send(&h.router)
        .await;
    refused.expect(StatusCode::SEE_OTHER);

    let callback = location(&refused);
    assert_eq!(
        query_param(&callback, "error").as_deref(),
        Some("invalid_scope")
    );
}

// ------------------------------------------------------- token endpoint

/// PKCE is mandatory, and the refusal has to name what is missing — a client
/// author debugging a bare `invalid_request` has nothing to go on.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_token_endpoint_refuses_a_request_without_pkce(pool: PgPool) {
    let h = harness(pool);
    let client_id = register(&h, "Test Agent", REDIRECT).await;

    let refused = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", "df_ac_whatever"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
        ])
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);

    // RFC 6749 §5.2 shape: `error` and `error_description` at the top level, not
    // the console's `{"error": {"code", "message"}}` envelope. A client handed
    // the wrong shape reports "unknown error".
    assert_eq!(refused.body["error"], "invalid_request");
    assert!(
        refused.body["error_description"]
            .as_str()
            .unwrap()
            .contains("code_verifier"),
        "{}",
        refused.text
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unsupported_grant_type_says_what_is_supported(pool: PgPool) {
    let h = harness(pool);
    let client_id = register(&h, "Test Agent", REDIRECT).await;

    let refused = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "password"),
            ("client_id", &client_id),
            ("username", "rob"),
            ("password", "hunter2"),
        ])
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);

    assert_eq!(refused.body["error"], "unsupported_grant_type");
    let description = refused.body["error_description"].as_str().unwrap();
    assert!(description.contains("authorization_code"), "{description}");
}

/// A stolen code is useless without the verifier only the initiating client
/// holds. This is the property PKCE exists for, checked at the HTTP edge.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_stolen_code_is_useless_without_the_verifier(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    let org_id = org_with_owner(&h, "acme", &rob).await;
    let client_id = register(&h, "Test Agent", REDIRECT).await;
    let (_, challenge) = pkce();

    let granted = Call::post("/oauth/authorize")
        .with_session(&rob.session)
        .form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "jobs:read"),
            ("org_id", &org_id.to_string()),
            ("decision", "allow"),
        ])
        .send(&h.router)
        .await;
    let code = query_param(&location(&granted), "code").unwrap();

    let refused = Call::post("/oauth/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", &"y".repeat(64)),
        ])
        .send(&h.router)
        .await;
    refused.expect(StatusCode::BAD_REQUEST);
    assert_eq!(refused.body["error"], "invalid_grant");
}

/// RFC 7009: always 200, even for a token that never existed. An endpoint that
/// reported otherwise would be an oracle for testing stolen tokens.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn revocation_is_silent_about_whether_the_token_existed(pool: PgPool) {
    let h = harness(pool);
    Call::post("/oauth/revoke")
        .form(&[("token", "df_at_never-existed")])
        .send(&h.router)
        .await
        .expect(StatusCode::OK);
}

// ------------------------------------------------------------ registration

/// Open registration is not unlimited registration, and the screening is what
/// keeps an authorization code from crossing the network in the clear.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn registration_screens_the_redirect_uris_it_will_accept(pool: PgPool) {
    let h = harness(pool);

    for (uri, why) in [
        ("http://app.example.com/cb", "cleartext to a public host"),
        ("https://*.example.com/cb", "a wildcard"),
        ("https://app.example.com/cb#frag", "a fragment"),
        ("not a uri", "not a URI at all"),
    ] {
        let refused = Call::post("/oauth/register")
            .json(serde_json::json!({ "redirect_uris": [uri] }))
            .send(&h.router)
            .await;
        refused.expect(StatusCode::BAD_REQUEST);
        assert_eq!(
            refused.body["error"], "invalid_request",
            "{why} was accepted"
        );
    }

    // Loopback over http is the case that must keep working: it is how every
    // CLI agent completes the flow (RFC 8252 §7.3).
    Call::post("/oauth/register")
        .json(serde_json::json!({ "redirect_uris": ["http://127.0.0.1:1455/cb"] }))
        .send(&h.router)
        .await
        .expect(StatusCode::CREATED);
}

/// A client is inert until a human consents, but registration still costs a
/// row, so the endpoint the plan puts behind a throttle is actually throttled.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_registered_client_is_usable_and_named_on_the_consent_screen(pool: PgPool) {
    let h = harness(pool);
    let rob = onboard(&h, "rob@acme.test").await;
    org_with_owner(&h, "acme", &rob).await;

    let registered = Call::post("/oauth/register")
        .json(serde_json::json!({
            "client_name": "<script>alert(1)</script>",
            "redirect_uris": [REDIRECT],
        }))
        .send(&h.router)
        .await;
    registered.expect(StatusCode::CREATED);
    assert_eq!(
        registered.body["token_endpoint_auth_method"], "none",
        "public client: PKCE is the proof of possession, not a secret"
    );

    let client_id = registered.body["client_id"].as_str().unwrap();
    let (_, challenge) = pkce();

    let page = Call::get(authorize_url(client_id, &challenge, "jobs:read", "s"))
        .with_session(&rob.session)
        .send(&h.router)
        .await;
    page.expect(StatusCode::OK);

    assert!(
        !page.text.contains("<script>alert(1)</script>"),
        "a self-asserted client name reached the consent page as markup"
    );
    assert!(page.text.contains("&lt;script&gt;"));
}
