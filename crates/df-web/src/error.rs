//! The console API's error envelope.
//!
//! Every failure leaves here as `{"error": {"code": …, "message": …}}` with a
//! status that matches. `code` is the stable branch point a UI switches on;
//! `message` is what it shows a human. That is the same split `df-core::Error`
//! already makes for agents, and it is deliberately the same envelope shape, so
//! a person debugging the console and a person debugging an agent are reading
//! the same thing.
//!
//! **Two rules about what goes in `message`.**
//!
//! A database error is never one of them. `df_core::Error::Db` carries table
//! and constraint names, which tell an attacker about a schema they cannot
//! otherwise see and tell a user nothing they can act on. It is logged in full
//! and reported as a flat internal error.
//!
//! An *identity* failure gets [`AuthError::public`] and nothing else. The full
//! variant distinguishes "no such user" from "wrong code" from "replayed code";
//! the caller must not be able to. That distinction is the account enumeration
//! oracle `df-auth` spends a whole module avoiding, and it would be
//! reintroduced here by one careless `to_string()`.
//!
//! The exception is an OAuth *protocol* error — `invalid_scope`,
//! `invalid_request`, `invalid_grant`. Those describe the caller's own request
//! rather than anyone's identity, so RFC 6749 §5.2 returns them verbatim and so
//! do we: there is nothing to enumerate, and "invalid_scope" with no further
//! word leaves an integrator with nothing to fix. `unknown scope "jobs:destroy";
//! supported scopes are …` is the whole difference between a five-second fix
//! and an afternoon.

use axum::response::{IntoResponse, Response};
use axum::Json;
use df_auth::AuthError;
use df_core::Error as CoreError;
use http::{header, HeaderValue, StatusCode};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Seconds until a throttled caller may retry, rendered as `Retry-After`.
    pub retry_after: Option<i64>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// No usable session. The console reads this and sends the user to sign in.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "sign in to continue",
        )
    }

    /// Signed in, but not allowed to do this.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// An unexpected failure. The detail is logged, never sent.
    pub fn internal(context: &str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, context, "console request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "something went wrong on our side; try again shortly",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(serde_json::json!({
                "error": { "code": self.code, "message": self.message },
            })),
        )
            .into_response();

        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }

        response
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        use CoreError::*;

        // These two never reach the caller. Everything else in `df-core::Error`
        // was written to be read by whoever hit it.
        //
        // `IsolationNotEnforced` is a startup assertion, so arriving here at all
        // would mean a server that promised to refuse to serve is serving. It is
        // logged in full and answered vaguely: its message names database roles
        // and tables, which is infrastructure detail no HTTP client should be
        // handed.
        match &e {
            Db(inner) => return ApiError::internal("df-core", inner),
            IsolationNotEnforced { .. } => return ApiError::internal("df-core", &e),
            _ => {}
        }

        let status = match &e {
            JobNotFound(_)
            | RepoNotFound(_)
            | RepoUnresolved { .. }
            | TeamNotFound { .. }
            | OrgNotFound(_) => StatusCode::NOT_FOUND,

            RepoSlugTaken(_)
            | RemoteTaken(..)
            | TeamSlugTaken(_)
            | TeamInUse { .. }
            | AlreadyAMember { .. }
            | LeaseHeld { .. }
            | LeaseNotHeld(_)
            | AlreadyClaimed { .. } => StatusCode::CONFLICT,

            // Gone, not Not Found: the link was real, and saying so is what
            // tells the holder to ask for a new one rather than re-check the URL.
            InviteInvalid => StatusCode::GONE,

            InviteWrongAccount { .. } => StatusCode::FORBIDDEN,

            WrongStatus { .. } | DependencyCycle(..) | Invalid(_) | NotAMember(_) => {
                StatusCode::BAD_REQUEST
            }

            Db(_) | IsolationNotEnforced { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        };

        ApiError::new(status, e.code(), e.to_string())
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        // Identity failures get the vague public string; protocol errors get
        // their own words. See the module docs for why the line falls there.
        let message = match e.oauth_code() {
            Some(_) => e.to_string(),
            None => e.public().to_string(),
        };
        let (status, code) = (
            StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            auth_code(&e),
        );

        if matches!(
            e,
            AuthError::Config(_) | AuthError::Crypto(_) | AuthError::Db(_)
        ) {
            return ApiError::internal("df-auth", e);
        }
        if let AuthError::Core(inner) = e {
            return ApiError::from(inner);
        }

        let retry_after = match e {
            AuthError::RateLimited { retry_after_secs } => Some(retry_after_secs),
            _ => None,
        };

        ApiError {
            status,
            code,
            message,
            retry_after,
        }
    }
}

impl From<df_billing::BillingError> for ApiError {
    fn from(e: df_billing::BillingError) -> Self {
        use df_billing::BillingError;

        match e {
            // Written for an agent that has to decide what to do next, and it
            // reads just as well to a person looking at the console — it names
            // what ran out and where to fix it. Passed through verbatim.
            quota @ BillingError::QuotaExceeded { .. } => ApiError::new(
                StatusCode::PAYMENT_REQUIRED,
                quota.code(),
                quota.to_string(),
            ),
            BillingError::Core(inner) => ApiError::from(inner),
        }
    }
}

/// A stable code per auth failure, coarser than the variant on purpose: the
/// credential failures collapse to one code for the same reason they collapse
/// to one message.
fn auth_code(e: &AuthError) -> &'static str {
    match e {
        AuthError::UnknownUser
        | AuthError::NoTotp
        | AuthError::BadTotpCode
        | AuthError::TotpReplay
        | AuthError::BadRecoveryCode
        | AuthError::Disabled => "invalid_credentials",

        AuthError::Expired | AuthError::AlreadyConsumed | AuthError::Revoked => {
            "credential_expired"
        }

        AuthError::WrongAudience => "wrong_audience",
        AuthError::NotAMember => "not_a_member",
        AuthError::SsoRequired => "sso_required",
        AuthError::RateLimited { .. } => "rate_limited",

        AuthError::InvalidRequest(_) => "invalid_request",
        AuthError::InvalidClient(_) => "invalid_client",
        AuthError::InvalidGrant(_) => "invalid_grant",
        AuthError::UnsupportedGrantType(_) => "unsupported_grant_type",
        AuthError::InvalidScope(_) => "invalid_scope",

        AuthError::Config(_) | AuthError::Crypto(_) | AuthError::Core(_) | AuthError::Db(_) => {
            "internal_error"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one regression this file exists to prevent. A `Db` error carries
    /// constraint and column names; if it ever reaches a response body, an
    /// attacker gets a free schema dump and the user gets nothing useful.
    #[test]
    fn database_errors_never_reach_the_caller() {
        let leaky = CoreError::Db(sqlx::Error::Protocol(
            "duplicate key value violates unique constraint \"org_invites_token_key\"".into(),
        ));
        let api = ApiError::from(leaky);

        assert_eq!(api.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api.code, "internal_error");
        assert!(
            !api.message.contains("org_invites"),
            "the schema leaked into the response: {}",
            api.message
        );
    }

    /// Every credential failure must be indistinguishable from the outside.
    #[test]
    fn credential_failures_are_one_answer() {
        let seen: Vec<(u16, &str, String)> = [
            AuthError::UnknownUser,
            AuthError::NoTotp,
            AuthError::BadTotpCode,
            AuthError::TotpReplay,
            AuthError::BadRecoveryCode,
            AuthError::Disabled,
        ]
        .into_iter()
        .map(|e| {
            let a = ApiError::from(e);
            (a.status.as_u16(), a.code, a.message)
        })
        .collect();

        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "login failures are distinguishable, which is an enumeration oracle: {seen:?}"
        );
        assert_eq!(seen[0].1, "invalid_credentials");
    }

    /// A protocol error describes the request, not the requester. Collapsing it
    /// to its bare code leaves an integrator with nothing to act on, and there
    /// is nothing to enumerate — the caller already knows what they sent.
    #[test]
    fn a_protocol_error_keeps_the_detail_that_makes_it_fixable() {
        let api = ApiError::from(AuthError::InvalidScope(
            r#"unknown scope "jobs:destroy"; supported scopes are jobs:read jobs:write"#.into(),
        ));

        assert_eq!(api.code, "invalid_scope");
        assert!(
            api.message.contains("jobs:read"),
            "the supported scopes were dropped: {}",
            api.message
        );
    }

    #[test]
    fn a_throttled_caller_is_told_when_to_come_back() {
        let api = ApiError::from(AuthError::RateLimited {
            retry_after_secs: 60,
        });
        assert_eq!(api.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(api.retry_after, Some(60));

        let response = api.into_response();
        assert_eq!(
            response.headers().get(header::RETRY_AFTER).unwrap(),
            "60",
            "a 429 without Retry-After leaves a client guessing"
        );
    }

    /// An invitation that has been spent is Gone, not Not Found: the difference
    /// is what tells the holder to ask for a new one rather than re-check the
    /// URL they were sent.
    #[test]
    fn a_spent_invitation_is_gone_and_a_taken_slug_is_a_conflict() {
        assert_eq!(
            ApiError::from(CoreError::InviteInvalid).status,
            StatusCode::GONE
        );
        assert_eq!(
            ApiError::from(CoreError::TeamSlugTaken("platform".into())).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::from(CoreError::TeamNotFound {
                slug: "nope".into(),
                known: "platform".into(),
            })
            .status,
            StatusCode::NOT_FOUND
        );
    }
}
