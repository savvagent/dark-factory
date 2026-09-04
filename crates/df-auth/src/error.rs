//! Authentication errors.
//!
//! Two audiences, and they must not see the same thing:
//!
//! - **The caller** gets [`AuthError::public`] — a deliberately vague string.
//!   Login failures are constant-shape whether the address is unknown, the
//!   account has no passkey, or the passkey ceremony failed. Any distinction
//!   between those is an account-enumeration oracle.
//! - **The log and the audit trail** get the full variant, which says exactly
//!   what happened, because an operator debugging a failed login should not
//!   have to guess.

pub type Result<T> = std::result::Result<T, AuthError>;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    // ---- credential failures: all of these are "invalid credentials" outside.
    #[error("no such user")]
    UnknownUser,

    #[error("account has no registered passkey")]
    NoPasskey,

    /// Every WebAuthn failure collapses here. The library distinguishes "wrong
    /// origin" from "user verification not performed" from "bad signature", and
    /// all of that is useful in a log and none of it is safe to hand back — it
    /// tells an attacker which part of a forgery to fix next.
    #[error("the authenticator's response was not accepted")]
    InvalidCredentials,

    /// The challenge is gone: expired, already spent, or never issued by this
    /// server. Single-use is what stops a captured ceremony being replayed.
    #[error("that sign-in attempt has expired; start again")]
    CeremonyExpired,

    #[error("that authenticator is already registered")]
    CredentialAlreadyRegistered,

    #[error("no such passkey on this account")]
    UnknownCredential,

    /// Removing the last passkey would lock the account out permanently: there
    /// is no email to recover through, and the click looks like tidying up.
    #[error("that is the only passkey on this account — register another before removing it")]
    LastPasskey,

    #[error("account is disabled")]
    Disabled,

    #[error("credential expired")]
    Expired,

    #[error("credential was already consumed")]
    AlreadyConsumed,

    #[error("credential revoked")]
    Revoked,

    #[error("token is not valid for this resource")]
    WrongAudience,

    #[error("user is not a member of that org")]
    NotAMember,

    #[error("this org requires single sign-on")]
    SsoRequired,

    // ---- rate limiting
    #[error("too many attempts; retry in {retry_after_secs}s")]
    RateLimited { retry_after_secs: i64 },

    // ---- OAuth protocol errors: these ARE returned verbatim, per RFC 6749 §5.2.
    // They describe the client's request, not the user's identity, so there is
    // no enumeration risk and a vague message would make integration impossible.
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    #[error("invalid_client: {0}")]
    InvalidClient(String),

    #[error("invalid_grant: {0}")]
    InvalidGrant(String),

    #[error("unsupported_grant_type: {0}")]
    UnsupportedGrantType(String),

    #[error("invalid_scope: {0}")]
    InvalidScope(String),

    // ---- operational
    #[error("configuration error: {0}")]
    Config(String),

    #[error("cryptographic failure: {0}")]
    Crypto(String),

    #[error(transparent)]
    Core(#[from] df_core::Error),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl AuthError {
    /// What the caller is told.
    ///
    /// Every identity-related failure collapses to one string. This is the
    /// enumeration defense: an attacker probing addresses must not be able to
    /// tell "no such user" from "wrong code", and a user who has lost their
    /// phone must not be told whether the address is registered.
    pub fn public(&self) -> &'static str {
        match self {
            AuthError::UnknownUser
            | AuthError::NoPasskey
            | AuthError::InvalidCredentials
            | AuthError::Disabled => "invalid credentials",

            // Not folded into "invalid credentials": these say *what to do*
            // rather than whether an account exists, and a user who has to
            // start the ceremony again needs to be told so.
            AuthError::CeremonyExpired => "that sign-in attempt expired; try again",
            AuthError::CredentialAlreadyRegistered => "that authenticator is already registered",
            AuthError::UnknownCredential => "no such passkey",
            AuthError::LastPasskey => {
                "that is the only passkey on this account — register another first"
            }

            AuthError::Expired | AuthError::AlreadyConsumed | AuthError::Revoked => {
                "credential is no longer valid"
            }

            AuthError::WrongAudience => "token is not valid for this resource",
            AuthError::NotAMember => "not a member of that organization",
            AuthError::SsoRequired => "this organization requires single sign-on",
            AuthError::RateLimited { .. } => "too many attempts",

            AuthError::InvalidRequest(_) => "invalid_request",
            AuthError::InvalidClient(_) => "invalid_client",
            AuthError::InvalidGrant(_) => "invalid_grant",
            AuthError::UnsupportedGrantType(_) => "unsupported_grant_type",
            AuthError::InvalidScope(_) => "invalid_scope",

            AuthError::Config(_) | AuthError::Crypto(_) | AuthError::Core(_) | AuthError::Db(_) => {
                "internal error"
            }
        }
    }

    /// The RFC 6749 §5.2 error code for the token endpoint, or `None` when this
    /// is not an OAuth protocol error.
    pub fn oauth_code(&self) -> Option<&'static str> {
        match self {
            AuthError::InvalidRequest(_) => Some("invalid_request"),
            AuthError::InvalidClient(_) => Some("invalid_client"),
            AuthError::InvalidGrant(_) => Some("invalid_grant"),
            AuthError::UnsupportedGrantType(_) => Some("unsupported_grant_type"),
            AuthError::InvalidScope(_) => Some("invalid_scope"),
            _ => None,
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> u16 {
        match self {
            AuthError::RateLimited { .. } => 429,
            AuthError::NotAMember | AuthError::SsoRequired => 403,
            AuthError::Config(_) | AuthError::Crypto(_) | AuthError::Core(_) | AuthError::Db(_) => {
                500
            }
            AuthError::InvalidClient(_) => 401,
            _ => 400,
        }
    }
}
