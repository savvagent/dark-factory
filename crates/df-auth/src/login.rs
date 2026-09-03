//! The human login front door.
//!
//! [`crate::totp`] verifies a code for a user that has already been identified.
//! This module is what sits in front of it: it takes an email address typed
//! into a form, and it must answer *the same way* whether that address belongs
//! to an account, belongs to a disabled account, belongs to an account that
//! never finished enrolling, or belongs to nobody at all.
//!
//! **Why that matters more here than in most products.** dark-factory has no
//! passwords, so there is no "forgot password" screen whose behaviour already
//! leaks membership. The login form is the only oracle an attacker has, and the
//! thing it would leak is which of an enterprise's employees have accounts —
//! which is a target list for the phishing campaign that comes next.
//!
//! Three things make the answer constant:
//!
//! 1. **One error.** Every failure below returns a variant whose
//!    [`AuthError::public`] is `"invalid credentials"`. Callers must render
//!    `public()`, never the variant.
//! 2. **Comparable work.** The unknown-address path runs the same throttle
//!    queries, the same TOTP arithmetic ([`totp::decoy_check`]), and writes the
//!    same audit row. It does not read a credential or record a consumed step,
//!    so the paths are not *identical* in time — see `decoy_check` — but the
//!    gap is small relative to the noise on any real network.
//! 3. **The same throttle.** An unknown address is rate-limited exactly like a
//!    known one, so an attacker cannot spray a directory faster than they can
//!    attack one account.

use df_core::audit::{action, Entry};
use df_core::ids::UserId;
use df_core::orgs::User;
use df_core::Db;
use serde::Serialize;

use crate::crypto::Cipher;
use crate::error::{AuthError, Result};
use crate::ratelimit;
use crate::sessions::{self, Session};
use crate::totp;

/// How the human proved who they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Totp,
    RecoveryCode,
}

/// A successful login.
pub struct LoggedIn {
    pub user: UserId,
    /// The session cookie value, handed over once.
    pub session_token: String,
    pub session: Session,
    pub method: Method,
    /// The user has no usable second factor and must enrol one before they can
    /// log in by the ordinary path again.
    ///
    /// Computed from the account's actual state rather than from which door
    /// they came through, because those differ: a recovery *code* leaves TOTP
    /// intact, a recovery *link* deliberately destroys it.
    pub must_enroll_totp: bool,
}

impl std::fmt::Debug for LoggedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggedIn")
            .field("user", &self.user)
            .field("session_token", &"<redacted>")
            .field("session", &self.session)
            .field("method", &self.method)
            .field("must_enroll_totp", &self.must_enroll_totp)
            .finish()
    }
}

/// Log in with an authenticator code.
pub async fn with_totp(
    db: &Db,
    cipher: &Cipher,
    email: &str,
    code: &str,
    issuer: &str,
    ip: Option<&str>,
) -> Result<LoggedIn> {
    let key = email.trim().to_lowercase();
    guard(db, &key, ip).await?;

    let outcome = match resolve_account(db, &key).await? {
        Some(user) => {
            totp::verify(db, cipher, user.id, &user.email, issuer, code, ip).await?;
            Ok(user.id)
        }
        None => {
            // Same arithmetic, same audit row, same error. See the module docs.
            totp::decoy_check(code);
            note_unknown(db, &key, ip, "totp").await;
            Err(AuthError::UnknownUser)
        }
    };

    finish(db, &key, ip, outcome, Method::Totp).await
}

/// Log in with one of the codes issued at enrollment.
///
/// The other half of "my phone is in a taxi", and the **only** self-service way
/// back in: there is no emailed recovery link, because there is no email.
///
/// Deliberately does **not** reset TOTP — the user still holds the secret, they
/// just cannot reach it right now, and destroying it would force a
/// re-enrollment they did not ask for. Someone who has lost the authenticator
/// itself needs `totp::reset`, which only an org admin can reach on their
/// behalf.
pub async fn with_recovery_code(
    db: &Db,
    email: &str,
    code: &str,
    ip: Option<&str>,
) -> Result<LoggedIn> {
    let key = email.trim().to_lowercase();
    guard(db, &key, ip).await?;

    let outcome = match resolve_account(db, &key).await? {
        Some(user) => {
            totp::consume_recovery_code(db, user.id, code, ip).await?;
            Ok(user.id)
        }
        None => {
            note_unknown(db, &key, ip, "recovery_code").await;
            Err(AuthError::UnknownUser)
        }
    };

    finish(db, &key, ip, outcome, Method::RecoveryCode).await
}

/// End a session and record it. The counterpart to every constructor above.
pub async fn logout(db: &Db, session_token: &str, ip: Option<&str>) -> Result<()> {
    // Resolve before revoking so the audit row names the user. A cookie that
    // resolves to nothing is still revoked below — logging out is not a
    // privileged operation and must not fail for a visitor holding a stale one.
    let user = sessions::resolve(db, session_token)
        .await
        .ok()
        .map(|s| s.user_id);

    sessions::revoke(db, session_token).await?;

    if let Some(user) = user {
        let _ = db
            .audit_global(
                Entry::new(action::LOGOUT)
                    .actor(user)
                    .from_request(ip, None),
            )
            .await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The shared shape
// ---------------------------------------------------------------------------

/// Throttle before touching a credential, on both the address and the source.
///
/// Address-only would let one attacker work through a directory a few tries per
/// account at a time; source-only would let a botnet through. Both, or neither
/// is worth having.
async fn guard(db: &Db, email_key: &str, ip: Option<&str>) -> Result<()> {
    ratelimit::check(db, &format!("login:email:{email_key}")).await?;
    if let Some(ip) = ip {
        ratelimit::check(db, &format!("login:ip:{ip}")).await?;
    }
    Ok(())
}

/// The account behind an address, if it can log in at all.
///
/// A disabled account resolves to `None` so it takes the unknown-address path
/// exactly: "your account was suspended" is not something a login form should
/// be willing to confirm to whoever is typing.
async fn resolve_account(db: &Db, email_key: &str) -> Result<Option<User>> {
    Ok(db
        .get_user_by_email(email_key)
        .await?
        .filter(|u| u.disabled_at.is_none()))
}

/// Record the attempt against both buckets, then turn a resolved user into a
/// session. One place, so no login path can forget half of it.
async fn finish(
    db: &Db,
    email_key: &str,
    ip: Option<&str>,
    outcome: Result<UserId>,
    method: Method,
) -> Result<LoggedIn> {
    let ok = outcome.is_ok();
    ratelimit::record(db, &format!("login:email:{email_key}"), ok).await?;
    if let Some(ip) = ip {
        ratelimit::record(db, &format!("login:ip:{ip}"), ok).await?;
    }

    let user = outcome?;
    let session = sessions::create(db, user).await?;

    Ok(LoggedIn {
        user,
        session_token: session.token,
        session: session.session,
        method,
        must_enroll_totp: !totp::has_confirmed_credential(db, user).await?,
    })
}

/// The audit row the unknown-address path writes, so the trail does not make
/// "no such user" visible by its absence. `actor_label` rather than `actor`
/// because there is no user id to name.
async fn note_unknown(db: &Db, email_key: &str, ip: Option<&str>, method: &str) {
    let entry = Entry::new(action::LOGIN_FAILED)
        .actor_label(email_key)
        .from_request(ip, None)
        .detail(serde_json::json!({ "method": method, "reason": "no such account" }));
    if let Err(e) = db.audit_global(entry).await {
        tracing::error!(error = %e, "failed to write audit event for a login attempt");
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The enumeration defense is one string, and it is the string callers
    /// render. If any of these ever diverge, the login form starts answering
    /// "does this address have an account?".
    #[test]
    fn every_login_failure_looks_identical_to_the_caller() {
        let expected = "invalid credentials";
        for e in [
            AuthError::UnknownUser,
            AuthError::NoTotp,
            AuthError::BadTotpCode,
            AuthError::TotpReplay,
            AuthError::BadRecoveryCode,
            AuthError::Disabled,
        ] {
            assert_eq!(e.public(), expected, "{e:?} leaks which failure it was");
        }
    }
}
