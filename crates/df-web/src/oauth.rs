//! The authorization server, over HTTP.
//!
//! `df-auth` decides everything here; this module is the transport, the consent
//! page, and nothing else. It lives in `df-web` rather than `df-mcp` because
//! `/oauth/authorize` is a *browser* surface that needs the console's session
//! cookie — it is the one place the two authentication layers meet, and it is
//! why the cookie is `SameSite=Lax` rather than `Strict`.
//!
//! ## The consent screen is a security control
//!
//! Client registration is open by design: an MCP client self-registers, and
//! requiring an admin to pre-create one would defeat the zero-install premise.
//! So anyone can register a client called "Claude Code" pointing at their own
//! redirect URI. Nothing in the protocol prevents that, which puts the entire
//! defense on this page — and it means the page must lead with **the redirect
//! host**, the one fact the user can actually judge, and treat `client_name` as
//! the attacker-controlled string it is.
//!
//! Everything rendered from the request is HTML-escaped through [`escape`]. A
//! client name is not markup.
//!
//! ## Error routing
//!
//! An OAuth failure goes to one of two places, and choosing wrongly is the
//! classic open-redirector bug:
//!
//! - **Before the redirect URI is validated** — unknown client, unregistered
//!   URI — the error is rendered as a page. Redirecting here would mean sending
//!   the user, and the `state` parameter, to precisely the destination we could
//!   not verify.
//! - **After it is validated**, errors go back to the client as query
//!   parameters, which is what lets an agent show a useful message instead of
//!   hanging on a callback that never arrives.

use axum::extract::{Form, Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use df_auth::error::AuthError;
use df_auth::{oauth, tokens};
use df_core::audit::{action, Entry};
use df_core::ids::OrgId;
use http::request::Parts;
use serde::Deserialize;

use crate::error::ApiError;
use crate::session::CurrentUser;
use crate::state::{client_ip, AppState};

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// `GET /.well-known/oauth-authorization-server` (RFC 8414).
///
/// Open, and necessarily so: it is what an unauthenticated client reads to find
/// out how to authenticate.
pub async fn as_metadata(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(oauth::as_metadata(&state.config.public_url))
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728).
///
/// Also served by `df-mcp`, which is where the `401` challenge points. Served
/// here too because some clients look for it beside the AS metadata rather than
/// beside the resource, and a client that cannot find this document cannot
/// start the flow at all.
pub async fn protected_resource_metadata(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(oauth::protected_resource_metadata(
        &state.config.resource_uri,
        &state.config.public_url,
    ))
}

// ---------------------------------------------------------------------------
// Dynamic client registration
// ---------------------------------------------------------------------------

/// `POST /oauth/register` (RFC 7591).
///
/// Open, and rate-limited per source address — the throttle `df-auth`'s own
/// comment says belongs at the HTTP layer, because that is the only layer that
/// knows where the request came from. Registration grants nothing on its own: a
/// client is inert until a human consents to it.
pub async fn register_client(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<oauth::RegistrationRequest>,
) -> Result<Response, OAuthError> {
    if let Some(ip) = client_ip(&parts, &state.config) {
        let bucket = format!("dcr:{ip}");
        df_auth::ratelimit::check(&state.db, &bucket).await?;
        df_auth::ratelimit::charge(&state.db, &bucket).await?;
    }

    let registered = oauth::register_client(&state.db, req).await?;

    let _ = state
        .db
        .audit_global(
            Entry::new(action::CLIENT_REGISTERED)
                .actor_label(registered.client_name.as_deref().unwrap_or("(unnamed)"))
                .target("client", registered.client_id.clone())
                .from_request(client_ip(&parts, &state.config).as_deref(), None),
        )
        .await;

    Ok((http::StatusCode::CREATED, Json(registered)).into_response())
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// The query string of an authorization request, exactly as OAuth defines it.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeParams {
    #[serde(default)]
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    /// Space-separated, per RFC 6749.
    #[serde(default)]
    pub scope: Option<String>,
    /// RFC 8707. Required: this server refuses to mint a token whose audience
    /// the client did not name.
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

impl AuthorizeParams {
    fn to_request(&self, fallback_resource: &str) -> oauth::AuthorizeRequest {
        oauth::AuthorizeRequest {
            client_id: self.client_id.clone(),
            redirect_uri: self.redirect_uri.clone(),
            code_challenge: self.code_challenge.clone(),
            code_challenge_method: self.code_challenge_method.clone(),
            scopes: self
                .scope
                .as_deref()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            // A client that omits `resource` is treated as naming ours, and
            // `validate_authorize` then checks it. Refusing outright would fail
            // clients that predate RFC 8707 for no security gain: there is
            // exactly one resource server here, so the only audience we would
            // ever mint for is this one.
            resource: self
                .resource
                .clone()
                .unwrap_or_else(|| fallback_resource.to_string()),
            state: self.state.clone(),
        }
    }
}

/// `GET /oauth/authorize` — render the consent screen.
///
/// A signed-out visitor is sent to the console's login page with `next` set to
/// this exact URL, so the flow resumes where it left off. That navigation is
/// the reason the session cookie is `SameSite=Lax`.
pub async fn authorize_page(
    State(state): State<AppState>,
    parts: Parts,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    let caller = match CurrentUser::from_request_parts_public(&parts, &state).await {
        Some(caller) => caller,
        None => {
            let next = urlencode(&format!(
                "/oauth/authorize?{}",
                parts.uri.query().unwrap_or_default()
            ));
            return axum::response::Redirect::to(&state.config.url(&format!("/login?next={next}")))
                .into_response();
        }
    };

    // Validated before anything is rendered. Every failure at this stage is a
    // page, never a redirect: the destination is what could not be verified.
    let client = match oauth::validate_authorize(
        &state.db,
        &params.to_request(&state.config.resource_uri),
        &state.config.resource_uri,
    )
    .await
    {
        Ok(client) => client,
        Err(e) => return error_page(&e),
    };

    if params.response_type != "code" {
        return redirect_error(
            &params,
            "unsupported_response_type",
            "this server issues authorization codes only",
        );
    }

    let orgs = match state.db.list_user_orgs(caller.user.id).await {
        Ok(orgs) => orgs,
        Err(e) => return ApiError::internal("list orgs for consent", e).into_response(),
    };

    if orgs.is_empty() {
        return error_page_html(
            "No organization yet",
            &format!(
                "{} is asking for access, but your account is not in any organization yet. \
                 Create one in the console first — a token is always scoped to exactly one.",
                escape(client.client_name.as_deref().unwrap_or("A client"))
            ),
        );
    }

    let scopes = oauth::validate_scopes(&params.to_request(&state.config.resource_uri).scopes)
        .unwrap_or_else(|_| {
            oauth::DEFAULT_SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

    Html(consent_html(
        &client,
        &params,
        &scopes,
        &orgs,
        &caller.user.email,
    ))
    .into_response()
}

/// The consent form's fields: the original request, plus the two decisions the
/// human makes.
///
/// The authorization parameters are written out rather than pulled in with
/// `#[serde(flatten)]`, which would be the obvious way and does not work:
/// `axum::Form` deserializes with `serde_urlencoded`, whose deserializer is not
/// self-describing, and `flatten` needs one. The failure is a runtime rejection
/// of every consent submission, not a compile error — so the duplication stays,
/// with [`ConsentForm::params`] as the single place it is undone.
#[derive(Debug, Deserialize)]
pub struct ConsentForm {
    #[serde(default)]
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// Which org this token will act in. A token opens exactly one.
    pub org_id: OrgId,
    /// "allow" or anything else, which is a denial.
    #[serde(default)]
    pub decision: String,
}

impl ConsentForm {
    fn params(&self) -> AuthorizeParams {
        AuthorizeParams {
            response_type: self.response_type.clone(),
            client_id: self.client_id.clone(),
            redirect_uri: self.redirect_uri.clone(),
            code_challenge: self.code_challenge.clone(),
            code_challenge_method: self.code_challenge_method.clone(),
            // A form always submits every field, so an omitted `state` arrives
            // as an empty string rather than as absent. Treating that as a real
            // value would append `&state=` to the callback, which some clients
            // compare against the nothing they sent and reject.
            scope: blank_to_none(self.scope.clone()),
            resource: blank_to_none(self.resource.clone()),
            state: blank_to_none(self.state.clone()),
        }
    }
}

fn blank_to_none(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// `POST /oauth/authorize` — the user's decision.
///
/// Cross-site protection is the session cookie's `SameSite=Lax`, which
/// withholds it on a cross-site `POST`: a form on an attacker's page submitting
/// here arrives without a session and is bounced to login rather than silently
/// consenting on the victim's behalf.
pub async fn authorize_decision(
    State(state): State<AppState>,
    caller: CurrentUser,
    parts: Parts,
    Form(form): Form<ConsentForm>,
) -> Response {
    let params = form.params();
    let mut req = params.to_request(&state.config.resource_uri);

    // Re-validated on the way in. The form is user-supplied and could have been
    // edited between render and submit; nothing about having rendered a page is
    // evidence about what came back.
    if let Err(e) = oauth::validate_authorize(&state.db, &req, &state.config.resource_uri).await {
        return error_page(&e);
    }

    // Normalize the same way `authorize_page` did before rendering the consent
    // screen: a client that omits `scope` gets `DEFAULT_SCOPES`, which is what
    // the human just looked at. Without this, the hidden form field resubmits
    // the *original*, empty `scope`, and the code issued below would carry
    // zero scopes while the page just displayed a list of grants — an agent
    // whose every tool call then fails, with no visible reason why.
    req.scopes = oauth::validate_scopes(&req.scopes).unwrap_or_else(|_| {
        oauth::DEFAULT_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect()
    });

    if form.decision != "allow" {
        return redirect_error(
            &params,
            "access_denied",
            "the user declined this authorization request",
        );
    }

    // The org must be one the caller actually belongs to. Without this check a
    // hand-edited form field is a cross-tenant token: everything downstream
    // trusts the org on the token, and this is where it is decided.
    let role = match state.db.member_role(form.org_id, caller.user.id).await {
        Ok(Some(role)) => role,
        Ok(None) => {
            return redirect_error(
                &params,
                "access_denied",
                "you are not a member of the selected organization",
            )
        }
        Err(e) => return ApiError::internal("check membership for consent", e).into_response(),
    };

    // `org:admin` is a real capability. A member consenting to it would hand a
    // client authority the human granting it does not have.
    if req.scopes.iter().any(|s| s == "org:admin") && !role.can_administer() {
        return redirect_error(
            &params,
            "invalid_scope",
            "org:admin needs an owner or admin of the selected organization",
        );
    }

    let code =
        match oauth::issue_authorization_code(&state.db, &req, caller.user.id, form.org_id).await {
            Ok(code) => code,
            Err(e) => return error_page(&e),
        };

    let _ = state
        .db
        .audit_for_org(
            form.org_id,
            Entry::new(action::AUTHORIZATION_GRANTED)
                .actor(caller.user.id)
                .target("client", params.client_id.clone())
                .from_request(client_ip(&parts, &state.config).as_deref(), None)
                .detail(serde_json::json!({ "scopes": req.scopes })),
        )
        .await;

    let mut location = append_query(&params.redirect_uri, &[("code", &code)]);
    if let Some(s) = &params.state {
        location = append_query(&location, &[("state", s)]);
    }

    // 303, not 302: the browser must turn a POST into a GET on the callback.
    (
        http::StatusCode::SEE_OTHER,
        [(http::header::LOCATION, location)],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Token and revocation
// ---------------------------------------------------------------------------

/// A token request. Form-encoded, per RFC 6749 §4.1.3 — not JSON, whatever a
/// modern instinct suggests. Clients send what the RFC says.
#[derive(Debug, Deserialize)]
pub struct TokenForm {
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

/// `POST /oauth/token`.
pub async fn token(
    State(state): State<AppState>,
    Form(form): Form<TokenForm>,
) -> Result<Response, OAuthError> {
    let resource = form
        .resource
        .clone()
        .unwrap_or_else(|| state.config.resource_uri.clone());

    let client_id = form
        .client_id
        .clone()
        .ok_or_else(|| AuthError::InvalidRequest("client_id is required".into()))?;

    let issued = match form.grant_type.as_str() {
        "authorization_code" => {
            let code = form
                .code
                .as_deref()
                .ok_or_else(|| AuthError::InvalidRequest("code is required".into()))?;
            let redirect_uri = form.redirect_uri.as_deref().ok_or_else(|| {
                AuthError::InvalidRequest("redirect_uri is required for authorization_code".into())
            })?;
            let verifier = form.code_verifier.as_deref().ok_or_else(|| {
                AuthError::InvalidRequest(
                    "code_verifier is required — PKCE S256 is mandatory".into(),
                )
            })?;

            let (issued, user, org) = oauth::redeem_code(
                &state.db,
                code,
                &client_id,
                redirect_uri,
                verifier,
                &resource,
            )
            .await?;

            let _ = state
                .db
                .audit_for_org(
                    org,
                    Entry::new(action::TOKEN_ISSUED)
                        .actor(user)
                        .target("client", client_id.clone()),
                )
                .await;

            issued
        }

        "refresh_token" => {
            let presented = form
                .refresh_token
                .as_deref()
                .ok_or_else(|| AuthError::InvalidRequest("refresh_token is required".into()))?;

            let (issued, user, org, _reused) =
                tokens::redeem_refresh(&state.db, presented, &client_id, &resource).await?;

            let _ = state
                .db
                .audit_for_org(
                    org,
                    Entry::new(action::TOKEN_REFRESHED)
                        .actor(user)
                        .target("client", client_id.clone()),
                )
                .await;

            issued
        }

        other => {
            return Err(OAuthError(AuthError::UnsupportedGrantType(format!(
                "{other:?}; this server implements authorization_code and refresh_token"
            ))))
        }
    };

    // RFC 6749 §5.1, including the no-store headers: an authorization response
    // must not sit in a shared cache.
    Ok((
        [
            (http::header::CACHE_CONTROL, "no-store"),
            (http::header::PRAGMA, "no-cache"),
        ],
        Json(serde_json::json!({
            "access_token": issued.access_token,
            "token_type": "Bearer",
            "expires_in": issued.expires_in,
            "refresh_token": issued.refresh_token,
            "scope": issued.scopes.join(" "),
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct RevokeForm {
    pub token: String,
}

/// `POST /oauth/revoke` (RFC 7009).
///
/// Always `200`, even for a token that never existed. The RFC requires it, and
/// the reason is the same one that makes `introspect` collapse unknown and
/// revoked: an endpoint that reports whether a string was a valid token is an
/// oracle for testing stolen ones.
pub async fn revoke(
    State(state): State<AppState>,
    Form(form): Form<RevokeForm>,
) -> Result<Response, OAuthError> {
    tokens::revoke_presented(&state.db, &form.token).await?;
    Ok(http::StatusCode::OK.into_response())
}

// ---------------------------------------------------------------------------
// Error shapes
// ---------------------------------------------------------------------------

/// An OAuth protocol error, rendered per RFC 6749 §5.2.
///
/// Deliberately not [`ApiError`]: the console's envelope is
/// `{"error": {"code", "message"}}`, and an OAuth client parses
/// `{"error", "error_description"}` at the top level. A client handed the wrong
/// shape reports "unknown error" and the user has nothing to go on.
#[derive(Debug)]
pub struct OAuthError(pub AuthError);

impl From<AuthError> for OAuthError {
    fn from(e: AuthError) -> Self {
        Self(e)
    }
}

impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let status =
            http::StatusCode::from_u16(self.0.status()).unwrap_or(http::StatusCode::BAD_REQUEST);
        let code = self.0.oauth_code().unwrap_or("invalid_request");

        // Protocol errors describe the client's *request*, not the user's
        // identity, so they are returned verbatim — there is no enumeration
        // risk and a vague message makes integration impossible. Anything
        // else falls back to the deliberately vague public string.
        let description = match self.0.oauth_code() {
            Some(_) => self.0.to_string(),
            None => self.0.public().to_string(),
        };

        let mut response = (
            status,
            Json(serde_json::json!({
                "error": code,
                "error_description": description,
            })),
        )
            .into_response();

        if status == http::StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                http::header::WWW_AUTHENTICATE,
                http::HeaderValue::from_static(r#"Basic realm="dark-factory""#),
            );
        }

        response
    }
}

/// Bounce an error back to a **validated** redirect URI.
fn redirect_error(params: &AuthorizeParams, code: &str, description: &str) -> Response {
    let mut location = append_query(
        &params.redirect_uri,
        &[("error", code), ("error_description", description)],
    );
    if let Some(s) = &params.state {
        location = append_query(&location, &[("state", s)]);
    }
    (
        http::StatusCode::SEE_OTHER,
        [(http::header::LOCATION, location)],
    )
        .into_response()
}

/// Render an error the user has to read, because it cannot safely be redirected.
fn error_page(e: &AuthError) -> Response {
    let detail = match e.oauth_code() {
        Some(_) => e.to_string(),
        None => e.public().to_string(),
    };
    error_page_html("This request could not be authorized", &escape(&detail))
}

fn error_page_html(title: &str, body_html: &str) -> Response {
    let title = escape(title);
    (
        http::StatusCode::BAD_REQUEST,
        Html(format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title>{STYLE}\
             <main><h1>{title}</h1><p>{body_html}</p>\
             <p class=note>Nothing has been authorized. You can close this window.</p></main>"
        )),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The consent page
// ---------------------------------------------------------------------------

const STYLE: &str = "<style>\
body{font:16px/1.5 system-ui,sans-serif;margin:0;background:#f6f7f9;color:#111}\
main{max-width:34rem;margin:3rem auto;padding:2rem;background:#fff;border-radius:12px;\
box-shadow:0 1px 3px rgba(0,0,0,.1)}\
h1{font-size:1.35rem;margin:0 0 1rem}\
.host{font:600 1.05rem ui-monospace,monospace;background:#eef;padding:.3rem .5rem;border-radius:6px}\
.name{color:#555}\
ul{padding-left:1.2rem}li{margin:.25rem 0}\
.note{color:#666;font-size:.875rem}\
label{display:block;margin:1rem 0 .35rem;font-weight:600}\
select{font:inherit;padding:.5rem;width:100%;border:1px solid #ccc;border-radius:8px}\
.row{display:flex;gap:.75rem;margin-top:1.75rem}\
button{font:inherit;padding:.6rem 1.2rem;border-radius:8px;border:1px solid #ccc;cursor:pointer}\
button.primary{background:#111;color:#fff;border-color:#111}\
</style>";

/// What each scope actually permits, in a sentence a person can weigh.
///
/// A consent screen listing `jobs:write` has not obtained informed consent from
/// anybody. If a scope is added to `KNOWN_SCOPES` without a line here it renders
/// as its bare name, which is ugly on purpose — the test at the bottom of this
/// file fails instead.
fn scope_description(scope: &str) -> &'static str {
    match scope {
        "jobs:read" => "See the work queue and job details",
        "jobs:write" => "Create, claim, update, and complete jobs",
        "repos:read" => "See which repositories are registered",
        "repos:write" => "Register repositories and change their settings",
        "messages" => "Read and send messages between agents",
        "trackers" => "Link jobs to issues in JIRA or GitHub",
        "org:admin" => "Administer the organization: members, teams, and connections",
        _ => "",
    }
}

fn consent_html(
    client: &oauth::Client,
    params: &AuthorizeParams,
    scopes: &[String],
    orgs: &[df_core::orgs::Membership],
    signed_in_as: &str,
) -> String {
    // The fact the user can actually judge. `client_name` is self-asserted
    // through open registration; the redirect host is where the code will
    // really be delivered, so it gets the visual weight.
    let host = url::Url::parse(&params.redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| params.redirect_uri.clone());

    let named = client
        .client_name
        .as_deref()
        .map(|n| format!("<p class=name>It calls itself {}.</p>", escape(n)))
        .unwrap_or_default();

    let scope_items = scopes
        .iter()
        .map(|s| {
            let description = scope_description(s);
            if description.is_empty() {
                format!("<li><code>{}</code></li>", escape(s))
            } else {
                format!(
                    "<li>{} <span class=note>({})</span></li>",
                    escape(description),
                    escape(s)
                )
            }
        })
        .collect::<String>();

    let org_options = orgs
        .iter()
        .map(|m| {
            format!(
                "<option value=\"{}\">{}</option>",
                escape(&m.org_id.to_string()),
                escape(&m.org_name)
            )
        })
        .collect::<String>();

    let hidden = [
        ("response_type", params.response_type.as_str()),
        ("client_id", params.client_id.as_str()),
        ("redirect_uri", params.redirect_uri.as_str()),
        ("code_challenge", params.code_challenge.as_str()),
        (
            "code_challenge_method",
            params.code_challenge_method.as_str(),
        ),
        ("scope", params.scope.as_deref().unwrap_or("")),
        ("resource", params.resource.as_deref().unwrap_or("")),
        ("state", params.state.as_deref().unwrap_or("")),
    ]
    .iter()
    .map(|(k, v)| format!("<input type=hidden name={k} value=\"{}\">", escape(v)))
    .collect::<String>();

    format!(
        "<!doctype html><meta charset=utf-8><title>Authorize access</title>{STYLE}\
         <main>\
         <h1>Authorize access to dark-factory</h1>\
         <p>An application running on <span class=host>{host}</span> is asking to \
            connect to your queue.</p>\
         {named}\
         <p class=note>Only continue if you started this from that application. \
            Any application can choose its own name.</p>\
         <p>It is asking to:</p><ul>{scope_items}</ul>\
         <form method=post action=\"/oauth/authorize\">{hidden}\
         <label for=org_id>Organization</label>\
         <select id=org_id name=org_id>{org_options}</select>\
         <p class=note>The token will act in this organization only, and cannot be \
            moved to another.</p>\
         <div class=row>\
         <button class=primary type=submit name=decision value=allow>Allow access</button>\
         <button type=submit name=decision value=deny>Cancel</button>\
         </div></form>\
         <p class=note>Signed in as {signed_in}.</p>\
         </main>",
        host = escape(&host),
        signed_in = escape(signed_in_as),
    )
}

/// Escape text for interpolation into HTML.
///
/// Covers attribute contexts too, which is why `"` and `'` are here: every
/// interpolation on the consent page is either element text or a
/// double-quoted attribute value.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-parameter value.
///
/// Hand-rolled against RFC 3986's unreserved set — everything outside it is
/// encoded, which is conservative and cannot under-encode. The failure this
/// guards against is real: an unencoded `state` containing `&` splits into two
/// parameters and the client's CSRF check silently compares the wrong string.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
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

/// Append `key=value` query parameters onto a redirect URI that may already
/// carry its own query string.
///
/// RFC 6749 §3.1.2 requires a registered redirect URI's existing query
/// component to be retained, with the response parameters appended to it. A
/// client registered at `https://app.example.com/cb?tenant=acme` still needs
/// `code`/`state` (or `error`/`error_description`) appended with `&`, not a
/// second `?` — `?tenant=acme?code=…` is not a query string any conformant
/// parser reads correctly, and the client would never see `code` or `state`.
/// `validate_registerable_redirect` accepts a query component (it only
/// rejects fragments, wildcards, and non-loopback cleartext), so this case is
/// reachable with any client that registers one.
fn append_query(redirect_uri: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = redirect_uri.to_string();
    let mut sep = if redirect_uri.contains('?') { '&' } else { '?' };
    for (key, value) in pairs {
        out.push(sep);
        out.push_str(key);
        out.push('=');
        out.push_str(&urlencode(value));
        sep = '&';
    }
    out
}

impl CurrentUser {
    /// Resolve a session without turning its absence into a rejection.
    ///
    /// The consent page needs "signed in, or send them to log in", not "signed
    /// in, or 401" — a `401` there is a dead end for a person who simply has not
    /// logged in yet.
    async fn from_request_parts_public(parts: &Parts, state: &AppState) -> Option<CurrentUser> {
        let token = crate::session::token_from(parts)?;
        let session = df_auth::sessions::resolve(&state.db, &token).await.ok()?;
        let user = state.db.get_user(session.user_id).await.ok()??;
        Some(CurrentUser { user, session })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escaping_neutralizes_a_client_name() {
        let attack = r#"<script>alert('xss')</script>"#;
        let escaped = escape(attack);
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'));
        assert_eq!(escaped, "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;");
    }

    /// A client name that breaks out of a double-quoted attribute is the other
    /// half of the same attack, and the one an element-only escaper misses.
    #[test]
    fn html_escaping_covers_attribute_contexts() {
        assert_eq!(
            escape(r#"" onmouseover="steal()"#),
            "&quot; onmouseover=&quot;steal()"
        );
    }

    #[test]
    fn url_encoding_protects_the_state_parameter() {
        assert_eq!(urlencode("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    /// A consent screen that lists `jobs:write` has not obtained informed
    /// consent from anyone. Every scope the authorization server will issue
    /// needs a sentence a person can actually weigh.
    #[test]
    fn every_issuable_scope_is_explained_in_words() {
        for scope in oauth::KNOWN_SCOPES {
            assert!(
                !scope_description(scope).is_empty(),
                "{scope} has no description, so the consent screen would show \
                 only its bare name"
            );
        }
    }

    /// The phishing defense, asserted: the redirect host must be in the page,
    /// and a hostile client name must not be able to escape into markup.
    #[test]
    fn the_consent_page_leads_with_the_redirect_host() {
        let client = oauth::Client {
            client_id: "df_client_x".into(),
            client_name: Some("<b>Claude Code</b>".into()),
            redirect_uris: vec!["http://127.0.0.1:1455/callback".into()],
            disabled: false,
        };
        let params = AuthorizeParams {
            response_type: "code".into(),
            client_id: "df_client_x".into(),
            redirect_uri: "http://127.0.0.1:1455/callback".into(),
            code_challenge: "x".repeat(43),
            code_challenge_method: "S256".into(),
            scope: Some("jobs:read".into()),
            resource: None,
            state: Some("opaque".into()),
        };
        let orgs = vec![df_core::orgs::Membership {
            org_id: OrgId::new(),
            user_id: df_core::ids::UserId::new(),
            role: df_core::orgs::Role::Owner,
            org_slug: "acme".into(),
            org_name: "Acme".into(),
            plan: df_core::orgs::Plan::Free,
        }];

        let html = consent_html(
            &client,
            &params,
            &["jobs:read".to_string()],
            &orgs,
            "rob@acme.test",
        );

        assert!(
            html.contains("127.0.0.1"),
            "the redirect host is the only fact a user can judge, and it is missing"
        );
        assert!(
            !html.contains("<b>Claude Code</b>"),
            "the client name reached the page as markup"
        );
        assert!(html.contains("&lt;b&gt;Claude Code&lt;/b&gt;"));
        assert!(
            html.contains("See the work queue"),
            "scopes must be rendered in words"
        );
        assert!(html.contains("name=org_id"), "no organization picker");
    }
}
