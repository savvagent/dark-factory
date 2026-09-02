//! Test harness: the assembled router, driven the way a browser drives it.
//!
//! Requests go through `tower::ServiceExt::oneshot` against the real router
//! rather than calling handlers directly, so extractors, path matching, method
//! routing, status codes, and `Set-Cookie` are all under test. A handler tested
//! in isolation cannot tell you that its route is mounted, that its extractor
//! resolves, or that its `404` is not a `403`.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::Router;
use base64::Engine;
use df_auth::crypto::Cipher;
use df_core::ids::{OrgId, UserId};
use df_core::orgs::Role;
use df_core::Db;
use df_web::mail::{CapturingMailer, Mail};
use df_web::{AppState, Config};
use http::{Request, Response, StatusCode};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

pub const RESOURCE: &str = "https://mcp.dark-factory.test/mcp";
pub const PUBLIC_URL: &str = "https://console.dark-factory.test";
pub const ISSUER: &str = "dark-factory";

pub struct Harness {
    pub db: Db,
    pub router: Router,
    pub mailer: Arc<CapturingMailer>,
    pub cipher: Cipher,
}

pub fn harness(pool: PgPool) -> Harness {
    let db = Db::from_pool(pool);
    let mailer = CapturingMailer::new();
    let state = AppState::new(
        db.clone(),
        cipher(),
        mailer.clone(),
        Config::new(PUBLIC_URL, RESOURCE),
    );

    Harness {
        db,
        router: df_web::router(state),
        mailer,
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
    pub totp: totp_rs::TOTP,
    pub recovery_codes: Vec<String>,
}

/// Take an address through signup → verification → enrollment, exactly as a
/// person would, and hand back a live session.
///
/// Deliberately not a shortcut that inserts rows: the point of most of these
/// tests is that the sequence works end to end, and a fixture that skips it
/// would test a state the product cannot actually reach.
pub async fn onboard(h: &Harness, email: &str) -> Account {
    Call::post("/api/auth/signup")
        .json(serde_json::json!({ "email": email, "name": "Test User" }))
        .send(&h.router)
        .await
        .expect(StatusCode::ACCEPTED);

    let token = link_token(&h.mailer.last().expect("no verification mail"));

    let verified = Call::post("/api/auth/verify")
        .json(serde_json::json!({ "token": token }))
        .send(&h.router)
        .await;
    verified.expect(StatusCode::OK);

    let session = verified
        .session_cookie()
        .expect("verification of a fresh account must open a session");

    let user = h.db.get_user_by_email(email).await.unwrap().unwrap().id;
    let (totp, recovery_codes) = enroll(h, &session, email).await;

    Account {
        user,
        email: email.to_string(),
        session,
        totp,
        recovery_codes,
    }
}

/// Enrol an authenticator through the API and confirm it.
///
/// Confirmation uses the *previous* step's code so the current step stays
/// unconsumed and available to log in with — using one step for both is a
/// replay, which is exactly what the credential refuses.
pub async fn enroll(h: &Harness, session: &str, email: &str) -> (totp_rs::TOTP, Vec<String>) {
    let started = Call::post("/api/me/totp")
        .with_session(session)
        .send(&h.router)
        .await;
    started.expect(StatusCode::OK);

    let manual_key = started.body["manualKey"].as_str().unwrap().to_string();
    let codes: Vec<String> = started.body["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();

    let secret = totp_rs::Secret::Encoded(manual_key).to_bytes().unwrap();
    let generator = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        secret,
        Some(ISSUER.to_string()),
        email.to_string(),
    )
    .unwrap();

    let previous_step = chrono::Utc::now().timestamp() as u64 - 30;
    Call::post("/api/me/totp/confirm")
        .with_session(session)
        .json(serde_json::json!({ "code": generator.generate(previous_step) }))
        .send(&h.router)
        .await
        .expect(StatusCode::NO_CONTENT);

    (generator, codes)
}

pub fn now_code(generator: &totp_rs::TOTP) -> String {
    generator.generate(chrono::Utc::now().timestamp() as u64)
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

/// Pull the `token=` query parameter out of the link in a message.
pub fn link_token(mail: &Mail) -> String {
    let start = mail
        .text
        .find("token=")
        .unwrap_or_else(|| panic!("no token in mail:\n{}", mail.text))
        + "token=".len();
    mail.text[start..]
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap()
        .to_string()
}
