//! The OAuth resource server: what stands between a bearer token and the queue.
//!
//! `df-auth` is the authorization server — it decides who may have a token and
//! what it says. This module is the *resource* server, and it has exactly three
//! jobs, in this order:
//!
//! 1. **Refuse an unauthenticated request in a way that teaches the client how
//!    to authenticate.** A `401` carrying
//!    `WWW-Authenticate: Bearer resource_metadata="…"` (RFC 9728) is the entire
//!    onboarding story: the user pastes one MCP URL into their agent, the agent
//!    gets this header, follows it to the metadata document, finds the
//!    authorization server, registers itself, and opens a browser. Nothing else
//!    is configured anywhere. Get this header wrong and the product's premise —
//!    one URL, no install — stops working, in a way that looks to the user like
//!    "the server is broken".
//! 2. **Enforce the audience.** [`df_auth::tokens::introspect`] refuses a token
//!    minted for any other resource. This is the confused-deputy defense and it
//!    is why the canonical URI is configuration rather than something derived
//!    from the request's `Host` header — a header an attacker controls is not a
//!    thing to compare an audience against.
//! 3. **Attach the principal to the request**, so handlers downstream have an
//!    org and a user without re-deciding authorization per tool.
//!
//! **The principal is per request, not per session.** An MCP session spans many
//! HTTP requests, and the token is re-introspected on every one. That costs an
//! indexed lookup and buys immediate revocation: a token revoked in the console
//! stops working on the agent's next call rather than whenever its session
//! happens to end.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use df_auth::tokens::{self, Principal};
use df_auth::AuthError;
use df_core::Db;
use http::{HeaderMap, StatusCode};

/// Everything the middleware needs, shared by the whole surface.
#[derive(Clone)]
pub struct ResourceServer {
    pub db: Db,
    /// This resource's canonical URI — the audience every token must name.
    ///
    /// Configuration, never derived from the request. A `Host` header is
    /// attacker-controlled, and an audience check against attacker-controlled
    /// input is not a check.
    pub resource_uri: String,
    /// Public base URL of the authorization server, for the discovery pointer.
    pub public_url: String,
}

impl ResourceServer {
    pub fn new(db: Db, resource_uri: impl Into<String>, public_url: impl Into<String>) -> Self {
        Self {
            db,
            resource_uri: resource_uri.into(),
            public_url: public_url.into(),
        }
    }

    /// Where an unauthenticated client is sent to find out what to do.
    pub fn metadata_url(&self) -> String {
        metadata_url(&self.public_url)
    }
}

/// Free-standing so the discovery pointer and the challenge that carries it can
/// be tested without a database handle. Neither depends on one, and a unit test
/// that has to build a connection pool to check a header string is a unit test
/// that will not be run.
fn metadata_url(public_url: &str) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource",
        public_url.trim_end_matches('/')
    )
}

/// Extract a bearer token from an `Authorization` header.
///
/// The scheme is compared case-insensitively because RFC 7235 says auth schemes
/// are case-insensitive and real clients send `bearer`. The token itself is
/// trimmed but otherwise untouched — it is compared by hash, so any
/// normalization here could only turn a valid token into an invalid one.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Build the `401` that tells a client where to authenticate.
///
/// `error` and `error_description` follow RFC 6750; `resource_metadata` follows
/// RFC 9728 and is the field MCP clients actually read.
fn challenge(metadata_url: &str, reason: &str) -> Response {
    let header = format!(
        r#"Bearer realm="dark-factory", error="invalid_token", error_description="{reason}", resource_metadata="{metadata_url}""#
    );

    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "invalid_token",
            "error_description": reason,
            "resource_metadata": metadata_url,
        })),
    )
        .into_response();

    // A header value that fails to build would silently drop the discovery
    // pointer and leave the client with a bare 401 it cannot act on, so fall
    // back to the pointer alone rather than to nothing.
    let value = http::HeaderValue::from_str(&header)
        .unwrap_or_else(|_| http::HeaderValue::from_static(r#"Bearer realm="dark-factory""#));
    response
        .headers_mut()
        .insert(http::header::WWW_AUTHENTICATE, value);

    response
}

/// Reject the request unless it carries a live token audienced for us.
///
/// On success the [`Principal`] is inserted into the request's extensions,
/// where the MCP transport carries it through to tool handlers as part of
/// `http::request::Parts`.
pub async fn require_bearer(
    State(rs): State<Arc<ResourceServer>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return challenge(&rs.metadata_url(), "an OAuth 2.1 bearer token is required");
    };

    match tokens::introspect(&rs.db, token, &rs.resource_uri).await {
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }

        // A database failure is not an authentication failure, and answering
        // `401` would be actively harmful: every connected agent would conclude
        // its token had died and stampede the authorization server at exactly
        // the moment the database is already unwell.
        Err(AuthError::Db(e)) => {
            tracing::error!(error = %e, "token introspection failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "temporarily_unavailable",
                    "error_description":
                        "could not verify the token right now; retry shortly. \
                         Your credentials are fine.",
                })),
            )
                .into_response()
        }

        Err(e) => {
            // The audience mismatch gets its own sentence. It is the one
            // failure a client can actually fix by changing what it asks for
            // rather than by re-authenticating, and it is otherwise a
            // maddening loop: obtain a token, get 401, obtain the same token
            // again, forever.
            let reason = match e {
                AuthError::WrongAudience => {
                    "this token was issued for a different resource; request one \
                     with the resource indicator this server advertises"
                }
                _ => e.public(),
            };
            challenge(&rs.metadata_url(), reason)
        }
    }
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728).
///
/// Served from here rather than alongside the authorization server's own
/// documents because it describes *this* resource, and because the `401` above
/// points at it — a discovery pointer whose target lives in a different crate's
/// router is a pointer that goes stale the first time the surfaces are split.
pub async fn protected_resource_metadata(
    State(rs): State<Arc<ResourceServer>>,
) -> Json<serde_json::Value> {
    Json(df_auth::oauth::protected_resource_metadata(
        &rs.resource_uri,
        &rs.public_url,
    ))
}

/// The authenticated caller of a tool call.
///
/// Handlers take `Extension(parts): Extension<http::request::Parts>` — the MCP
/// transport's only channel from HTTP into a tool — and pull the principal back
/// out with this.
pub fn principal_from(parts: &http::request::Parts) -> Option<Principal> {
    parts.extensions.get::<Principal>().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        for prefix in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            assert_eq!(
                bearer(&headers(&format!("{prefix} df_at_abc"))),
                Some("df_at_abc"),
                "{prefix} should be accepted"
            );
        }
    }

    #[test]
    fn other_schemes_and_malformed_headers_carry_no_token() {
        for bad in ["Basic dXNlcjpwdw==", "df_at_abc", "Bearer", "Bearer   ", ""] {
            assert_eq!(bearer(&headers(bad)), None, "{bad:?} should yield no token");
        }
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    /// The header a client cannot follow is the header that breaks onboarding.
    /// Every field an MCP client reads has to be present and well-formed.
    #[test]
    fn the_challenge_points_at_the_metadata_document() {
        let response = challenge(
            &metadata_url("https://mcp.example.com"),
            "an OAuth 2.1 bearer token is required",
        );

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let header = response
            .headers()
            .get(http::header::WWW_AUTHENTICATE)
            .expect("no WWW-Authenticate header")
            .to_str()
            .unwrap();

        assert!(header.starts_with("Bearer "));
        assert!(header.contains(r#"error="invalid_token""#));
        assert!(header.contains(
            r#"resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#
        ));
    }

    /// A trailing slash on the configured public URL must not produce a
    /// double-slashed metadata URL — some clients normalize that away and some
    /// fetch it verbatim and 404.
    #[test]
    fn the_metadata_url_survives_a_trailing_slash() {
        assert_eq!(
            metadata_url("https://mcp.example.com/"),
            metadata_url("https://mcp.example.com"),
            "a configured trailing slash must not produce a double-slashed URL — \
             some clients normalize that away and some fetch it verbatim and 404"
        );
    }
}
