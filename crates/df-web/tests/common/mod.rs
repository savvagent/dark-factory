//! Test harness: the assembled router, driven the way a browser drives it.
//!
//! Requests go through `tower::ServiceExt::oneshot` against the real router
//! rather than calling handlers directly, so extractors, path matching, method
//! routing, status codes, and `Set-Cookie` are all under test. A handler tested
//! in isolation cannot tell you that its route is mounted, that its extractor
//! resolves, or that its `404` is not a `403`.

#![allow(dead_code)]

use axum::body::Body;
use axum::Router;
use base64::Engine;
use df_core::crypto::Cipher;
use df_core::ids::{OrgId, UserId};
use df_core::orgs::Role;
use df_core::Db;
use df_web::{AppState, Config};
use http::{Request, Response, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use webauthn_authenticator_rs::softtoken::SoftToken;
use webauthn_authenticator_rs::WebauthnAuthenticator;

pub const RESOURCE: &str = "https://mcp.dark-factory.test/mcp";
pub const PUBLIC_URL: &str = "https://console.dark-factory.test";
pub const ISSUER: &str = "dark-factory";

pub struct Harness {
    pub db: Db,
    pub router: Router,
    pub cipher: Cipher,
}

pub fn harness(pool: PgPool) -> Harness {
    let db = Db::from_pool(pool);
    let config = Config::new(PUBLIC_URL, RESOURCE);
    let webauthn = df_web::relying_party(&config).expect("relying party");
    let state = AppState::new(db.clone(), cipher(), webauthn, config);

    Harness {
        db,
        router: df_web::router(state),
        cipher: cipher(),
    }
}

/// A harness whose deployment can actually take an admin through connecting a
/// tracker.
///
/// The plain [`harness`] configures no provider, which is a real deployment
/// shape worth testing (and what `a_deployment_that_cannot_connect_a_provider_says_so`
/// asserts) — but it means every connect request is refused for the deployment's
/// gap before the request itself is ever looked at. This one gets past that.
pub fn harness_with_trackers(pool: PgPool) -> Harness {
    let db = Db::from_pool(pool);
    let mut config = Config::new(PUBLIC_URL, RESOURCE);
    config.github_app_slug = Some("dark-factory".into());
    config.github_app_client_id = Some("gh-client".into());
    config.github_app_client_secret = Some("gh-secret".into());
    config.jira_client_id = Some("jira-client".into());
    config.jira_client_secret = Some("jira-secret".into());
    let webauthn = df_web::relying_party(&config).expect("relying party");
    let state = AppState::new(db.clone(), cipher(), webauthn, config);

    Harness {
        db,
        router: df_web::router(state),
        cipher: cipher(),
    }
}

pub fn cipher() -> Cipher {
    Cipher::from_base64_key(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

/// A response, already read into memory so a test can assert on both the status
/// and the body without threading a body future through every assertion.
pub struct Reply {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Value,
    /// The raw body, for the endpoints that answer with HTML.
    pub text: String,
}

impl Reply {
    /// The session cookie value this response set, if it set one.
    pub fn session_cookie(&self) -> Option<String> {
        self.headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                let value = v.strip_prefix("__Host-df_session=")?;
                let value = value.split(';').next()?;
                (!value.is_empty()).then(|| value.to_string())
            })
    }

    pub fn error_code(&self) -> Option<&str> {
        self.body.get("error")?.get("code")?.as_str()
    }

    /// Assert a status, printing the body when it does not match — a bare
    /// "expected 200, got 400" from an API test is a test that wastes an hour.
    pub fn expect(&self, status: StatusCode) -> &Self {
        assert_eq!(
            self.status,
            status,
            "unexpected status; body was: {}",
            if self.text.is_empty() {
                "(empty)"
            } else {
                &self.text
            }
        );
        self
    }
}

/// A request under construction.
pub struct Call {
    method: http::Method,
    uri: String,
    session: Option<String>,
    body: Option<Body>,
    content_type: Option<&'static str>,
}

impl Call {
    pub fn get(uri: impl Into<String>) -> Self {
        Self::new(http::Method::GET, uri)
    }
    pub fn post(uri: impl Into<String>) -> Self {
        Self::new(http::Method::POST, uri)
    }
    pub fn put(uri: impl Into<String>) -> Self {
        Self::new(http::Method::PUT, uri)
    }
    pub fn patch(uri: impl Into<String>) -> Self {
        Self::new(http::Method::PATCH, uri)
    }
    pub fn delete(uri: impl Into<String>) -> Self {
        Self::new(http::Method::DELETE, uri)
    }

    fn new(method: http::Method, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            session: None,
            body: None,
            content_type: None,
        }
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(Body::from(serde_json::to_vec(&body).unwrap()));
        self.content_type = Some("application/json");
        self
    }

    /// A form-encoded body — what the OAuth endpoints take, per RFC 6749.
    pub fn form(mut self, pairs: &[(&str, &str)]) -> Self {
        let encoded = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.body = Some(Body::from(encoded));
        self.content_type = Some("application/x-www-form-urlencoded");
        self
    }

    pub fn with_session(mut self, token: &str) -> Self {
        self.session = Some(token.to_string());
        self
    }

    pub async fn send(self, router: &Router) -> Reply {
        let mut builder = Request::builder().method(self.method).uri(&self.uri);

        if let Some(ct) = self.content_type {
            builder = builder.header(http::header::CONTENT_TYPE, ct);
        }
        if let Some(token) = &self.session {
            builder = builder.header(
                http::header::COOKIE,
                format!("__Host-df_session={token}; theme=dark"),
            );
        }

        let request = builder.body(self.body.unwrap_or_else(Body::empty)).unwrap();
        let response: Response<Body> = router.clone().oneshot(request).await.unwrap();

        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        Reply {
            status,
            headers,
            body,
            text,
        }
    }
}

fn urlencode(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Account fixtures
// ---------------------------------------------------------------------------

/// An account that has been through the whole front door: signed up, verified,
/// enrolled, signed in.
pub struct Account {
    pub user: UserId,
    pub email: String,
    pub session: String,
    /// The account's authenticator, kept so a test can sign in again.
    pub auth: Authenticator,
    /// base64url, for `allowCredentials`.
    pub credential_id: String,
}

/// A software authenticator, standing in for a browser's.
///
/// `SoftToken` produces real COSE signatures over the challenges this server
/// issued, so these tests exercise the actual verification path. It cannot hold
/// *discoverable* credentials — see `df-auth`'s `tests/passkeys.rs` for the full
/// note — so [`soften`] and [`offer`] adjust what is handed to it. Only what
/// the fake authenticator sees is adjusted; every server-side step is the
/// production one.
pub type Authenticator = WebauthnAuthenticator<SoftToken>;

pub fn authenticator() -> Authenticator {
    WebauthnAuthenticator::new(SoftToken::new(true).unwrap().0)
}

/// Drop the resident-key requirement before handing a challenge to SoftToken.
fn soften(mut challenge: Value) -> Value {
    if let Some(sel) = challenge
        .get_mut("publicKey")
        .and_then(|pk| pk.get_mut("authenticatorSelection"))
    {
        sel["requireResidentKey"] = Value::Bool(false);
        sel["residentKey"] = Value::Null;
    }
    challenge
}

/// Name a credential in `allowCredentials`, so a token holding no discoverable
/// credentials can find the right key. Production sends this list empty.
fn offer(mut challenge: Value, credential_id: &str) -> Value {
    challenge["publicKey"]["allowCredentials"] = serde_json::json!([
        { "type": "public-key", "id": credential_id }
    ]);
    challenge
}

/// Create an account the way a person does: register a passkey, get a session.
///
/// Deliberately not a shortcut that inserts rows. The point of most of these
/// tests is that the sequence works end to end, and a fixture that skipped it
/// would test a state the product cannot reach.
pub async fn onboard(h: &Harness, email: &str) -> Account {
    let mut auth = authenticator();

    let started = Call::post("/api/auth/signup/start").send(&h.router).await;
    started.expect(StatusCode::OK);
    assert!(
        started.session_cookie().is_none(),
        "a challenge must not open a session"
    );

    let ceremony_id = started.body["ceremonyId"].as_str().unwrap().to_string();
    let challenge: webauthn_rs::prelude::CreationChallengeResponse =
        serde_json::from_value(soften(started.body["challenge"].clone())).unwrap();

    let credential = auth
        .do_registration(
            webauthn_rs::prelude::Url::parse(PUBLIC_URL).unwrap(),
            challenge,
        )
        .expect("the authenticator refused the registration challenge");

    let finished = Call::post("/api/auth/signup/finish")
        .json(serde_json::json!({
            "ceremonyId": ceremony_id,
            "credential": credential,
            "nickname": "test key",
        }))
        .send(&h.router)
        .await;
    finished.expect(StatusCode::OK);

    let session = finished
        .session_cookie()
        .expect("finishing signup must open the account's first session");

    let user: UserId = finished.body["user"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Every test that predates passkeys assumes an addressable account, and an
    // invitation names an address — so set one here rather than in each test.
    Call::patch("/api/me")
        .with_session(&session)
        .json(serde_json::json!({ "email": email, "name": "Test User" }))
        .send(&h.router)
        .await
        .expect(StatusCode::OK);

    let credential_id = credential_id_of(h, user).await;

    Account {
        user,
        email: email.to_string(),
        session,
        auth,
        credential_id,
    }
}

/// The base64url credential id of an account's first passkey, for `offer`.
async fn credential_id_of(h: &Harness, user: UserId) -> String {
    use base64::Engine;
    let raw: Vec<u8> = sqlx::query_scalar(
        "SELECT credential_id FROM passkeys WHERE user_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(user)
    .fetch_one(h.db.pool())
    .await
    .unwrap();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Drive a registration ceremony to a `…/finish` endpoint, merging any extra
/// fields the endpoint needs (a claim code, for instance).
pub async fn finish_registration(
    h: &Harness,
    auth: &mut Authenticator,
    finish_path: &str,
    started: &Value,
    extra: Value,
) -> Reply {
    let ceremony_id = started["ceremonyId"].as_str().unwrap().to_string();
    let challenge: webauthn_rs::prelude::CreationChallengeResponse =
        serde_json::from_value(soften(started["challenge"].clone())).unwrap();

    let credential = auth
        .do_registration(
            webauthn_rs::prelude::Url::parse(PUBLIC_URL).unwrap(),
            challenge,
        )
        .expect("the authenticator refused the registration challenge");

    let mut body = serde_json::json!({
        "ceremonyId": ceremony_id,
        "credential": credential,
    });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }

    Call::post(finish_path.to_string())
        .json(body)
        .send(&h.router)
        .await
}

/// Sign in again with an account's own authenticator.
pub async fn sign_in(h: &Harness, account: &mut Account) -> Reply {
    let started = Call::post("/api/auth/login/start").send(&h.router).await;
    started.expect(StatusCode::OK);

    let ceremony_id = started.body["ceremonyId"].as_str().unwrap().to_string();
    let challenge: webauthn_rs::prelude::RequestChallengeResponse = serde_json::from_value(offer(
        started.body["challenge"].clone(),
        &account.credential_id,
    ))
    .unwrap();

    let credential = account
        .auth
        .do_authentication(
            webauthn_rs::prelude::Url::parse(PUBLIC_URL).unwrap(),
            challenge,
        )
        .expect("the authenticator refused the sign-in challenge");

    Call::post("/api/auth/login/finish")
        .json(serde_json::json!({ "ceremonyId": ceremony_id, "credential": credential }))
        .send(&h.router)
        .await
}
/// An org with `owner` as its owner.
pub async fn org_with_owner(h: &Harness, slug: &str, owner: &Account) -> OrgId {
    let created = Call::post("/api/orgs")
        .with_session(&owner.session)
        .json(serde_json::json!({ "slug": slug, "name": slug }))
        .send(&h.router)
        .await;
    created.expect(StatusCode::CREATED);
    created.body["id"].as_str().unwrap().parse().unwrap()
}

/// Add someone to an org directly, for tests about what a role may do rather
/// than about how someone got it.
pub async fn add_member(h: &Harness, org: OrgId, user: UserId, role: Role) {
    h.db.add_member(org, user, role).await.unwrap();
}
