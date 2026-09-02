//! Turning domain failures into something an agent can act on.
//!
//! The reader of every string in this file is an LLM that has never seen the
//! documentation, is holding a partly-wrong model of the server's state, and
//! must decide within one turn whether to retry, change the arguments, or give
//! up and ask a human. That audience changes what a good error is:
//!
//! - **Say what was wrong, what was valid, and what to call next.** `df-core`
//!   already writes its messages this way — `RepoUnresolved` lists the
//!   registered slugs, `LeaseHeld` names the holder and the expiry — so this
//!   module's job is to carry them through intact rather than flatten them into
//!   "bad request".
//! - **Put the branch point in machine-readable data, not in prose.** An agent
//!   matching on message text breaks the first time someone rewords one.
//!   `code` is stable; `retriable` answers the only question the agent has to
//!   resolve before its next action.
//! - **Never leak across the tenant boundary.** A message may name the caller's
//!   own repos and jobs. It may never name anything it took another org's data
//!   to know, which is why these are built from `df-core` errors that were
//!   themselves produced inside a pinned transaction.

use df_core::Error as CoreError;
use rmcp::model::{ErrorCode, ErrorData};

/// Convert a domain error into the MCP error envelope.
///
/// The JSON-RPC code is coarse on purpose: agents branch on `data.code`, and
/// spreading the distinction across two vocabularies would mean maintaining
/// both. Only the "your arguments are wrong" / "the server broke" split is
/// worth expressing here, because clients treat those differently at the
/// transport level.
pub fn from_core(e: &CoreError) -> ErrorData {
    let code = match e {
        // A database failure is ours, not the caller's. Reporting it as an
        // argument error would send an agent into a rewrite loop over a request
        // that was fine.
        CoreError::Db(_) => ErrorCode::INTERNAL_ERROR,
        _ => ErrorCode::INVALID_PARAMS,
    };

    // The internal message of a database error is not for the caller: it can
    // carry table names, constraint names, and fragments of SQL, none of which
    // an agent can act on and some of which describe other tenants' schema
    // surface. Log it, return a generic sentence.
    let message = match e {
        CoreError::Db(inner) => {
            tracing::error!(error = %inner, "database failure surfaced to an MCP caller");
            "the server could not complete this call; retry shortly".to_string()
        }
        other => other.to_string(),
    };

    ErrorData::new(
        code,
        message,
        Some(serde_json::json!({
            "code": e.code(),
            "retriable": e.retriable(),
        })),
    )
}

/// Convert an authentication or authorization failure.
///
/// Uses [`df_auth::AuthError::public`] rather than the variant's own message,
/// for the same reason the login form does: the distinctions between "no such
/// token", "revoked", and "expired" are an oracle, and an agent cannot act on
/// them differently anyway — every one of them means "get a new token".
///
/// The exception is a missing scope, which is genuinely actionable: the agent
/// must re-authorize asking for more, and it cannot do that without being told
/// which scope it lacks.
pub fn from_auth(e: &df_auth::AuthError) -> ErrorData {
    let (code, message, retriable) = match e {
        df_auth::AuthError::InvalidScope(detail) => {
            (ErrorCode::INVALID_REQUEST, detail.clone(), false)
        }
        df_auth::AuthError::RateLimited { retry_after_secs } => (
            ErrorCode::INVALID_REQUEST,
            format!("too many attempts; retry in {retry_after_secs}s"),
            true,
        ),
        other => (
            ErrorCode::INVALID_REQUEST,
            other.public().to_string(),
            false,
        ),
    };

    ErrorData::new(
        code,
        message,
        Some(serde_json::json!({
            "code": auth_code(e),
            "retriable": retriable,
        })),
    )
}

/// A stable machine-readable code for the auth failures an MCP caller can see.
fn auth_code(e: &df_auth::AuthError) -> &'static str {
    use df_auth::AuthError as A;
    match e {
        A::InvalidScope(_) => "insufficient_scope",
        A::WrongAudience => "wrong_audience",
        A::Expired => "token_expired",
        A::Revoked => "token_revoked",
        A::RateLimited { .. } => "rate_limited",
        A::NotAMember => "not_a_member",
        _ => "unauthorized",
    }
}

/// Convert a metering failure.
///
/// A quota refusal is not an argument error and must not read like one: the
/// call was well-formed and the agent should stop rather than rewrite it. The
/// message names the plan, the limit, and the URL a human goes to, because an
/// agent that cannot say *what to do about it* just retries.
pub fn from_billing(e: &df_billing::BillingError) -> ErrorData {
    if let df_billing::BillingError::Core(inner) = e {
        return from_core(inner);
    }

    ErrorData::new(
        ErrorCode::INVALID_REQUEST,
        e.to_string(),
        Some(serde_json::json!({
            "code": e.code(),
            "retriable": e.retriable(),
        })),
    )
}

/// The error a tool returns when it cannot find an authenticated caller.
///
/// This should be unreachable in production — the middleware refuses the
/// request with a `401` long before a handler runs — so reaching it means the
/// server was assembled without [`crate::auth::require_bearer`]. Say that,
/// loudly, rather than reporting it as the caller's problem: an agent debugging
/// its own arguments against a misconfigured server will never get anywhere.
pub fn unauthenticated() -> ErrorData {
    ErrorData::internal_error(
        "this request carried no authenticated principal, which means the MCP surface \
         was mounted without its resource-server middleware; this is a server \
         misconfiguration, not a problem with your call",
        Some(serde_json::json!({ "code": "unauthenticated", "retriable": false })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use df_core::ids::JobId;

    /// The two fields an agent branches on have to be present on every error,
    /// or the "check `retriable` before retrying" contract is a lie.
    #[test]
    fn every_error_carries_a_code_and_a_retriable_flag() {
        let errors = [
            CoreError::JobNotFound(JobId::from("job-1")),
            CoreError::Invalid("nope".into()),
            CoreError::LeaseHeld {
                branch: "main".into(),
                holder: "agent-a".into(),
                expires_at: chrono::Utc::now(),
            },
        ];

        for e in &errors {
            let data = from_core(e).data.expect("no data payload");
            assert_eq!(data["code"], e.code());
            assert_eq!(data["retriable"], e.retriable());
        }
    }

    /// `df-core` writes errors that name the alternatives. Flattening that away
    /// here would undo the whole point of writing them that way.
    #[test]
    fn the_actionable_detail_survives_conversion() {
        let e = CoreError::RepoUnresolved {
            attempted: "git@github.com:acme/nope.git".into(),
            known: "api, web".into(),
        };
        let converted = from_core(&e);
        assert!(converted.message.contains("api, web"));
        assert!(converted.message.contains("register_repo"));
    }

    /// A database error's own text can carry schema detail and is useless to an
    /// agent besides. It must not reach the wire.
    #[test]
    fn database_internals_do_not_reach_the_caller() {
        let e = CoreError::Db(sqlx::Error::RowNotFound);
        let converted = from_core(&e);
        assert_eq!(converted.code, ErrorCode::INTERNAL_ERROR);
        assert!(!converted.message.to_lowercase().contains("row"));
        assert_eq!(converted.data.unwrap()["retriable"], true);
    }

    /// A refusal an agent cannot fix by trying again has to say so, and say
    /// where a human can fix it, or the agent retries until something gives up.
    #[test]
    fn a_quota_refusal_is_actionable_and_not_retriable() {
        let e = from_billing(&df_billing::BillingError::QuotaExceeded {
            tool: "add_job".into(),
            used: 500,
            included: 500,
            plan: "Free".into(),
            upgrade_url: "https://example.test/settings/billing".into(),
        });

        let data = e.data.as_ref().unwrap();
        assert_eq!(data["code"], "quota_exceeded");
        assert_eq!(data["retriable"], false);
        assert!(e.message.contains("add_job"));
        assert!(e.message.contains("Free"));
        assert!(e.message.contains("https://example.test/settings/billing"));
        assert!(
            e.message.contains("Reads still work"),
            "the caller needs to know what it can still do"
        );
    }

    /// A database failure that reaches us through billing is still a database
    /// failure, and must not be reported as a quota problem.
    #[test]
    fn a_core_failure_under_billing_keeps_its_own_identity() {
        let e = from_billing(&df_billing::BillingError::Core(CoreError::Db(
            sqlx::Error::RowNotFound,
        )));
        assert_eq!(e.code, ErrorCode::INTERNAL_ERROR);
    }

    /// Credential failures collapse to one answer; a missing scope does not,
    /// because naming the scope is the only way the agent can fix it.
    #[test]
    fn credential_failures_collapse_but_scopes_are_named() {
        let revoked = from_auth(&df_auth::AuthError::Revoked);
        let expired = from_auth(&df_auth::AuthError::Expired);
        assert_eq!(revoked.message, expired.message);

        let scope = from_auth(&df_auth::AuthError::InvalidScope(
            "this token lacks the jobs:write scope".into(),
        ));
        assert!(scope.message.contains("jobs:write"));
        assert_eq!(scope.data.unwrap()["code"], "insufficient_scope");
    }
}
