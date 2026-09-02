//! Single-use email links.
//!
//! Three flows need the same primitive — prove control of an address, recover
//! an account whose authenticator is gone, accept an invitation — and all three
//! reduce to: mint a high-entropy token, mail it, accept it once, briefly.
//!
//! **Why a stored token rather than a signed one.** `DF_SIGNING_KEY` exists and
//! a signed, self-describing link would need no table. It would also be
//! unrevocable and un-single-usable without a table anyway — the moment you
//! need "this link has been used", you are storing state, and a signature has
//! bought nothing but a second failure mode. So these are random tokens stored
//! as SHA-256 hashes, exactly like every other credential here.
//!
//! **Ten minutes, and one live link per purpose.** A recovery link is a
//! password-reset link by another name: whoever holds it owns the account. It
//! is mailed in the clear, sits in an inbox indefinitely, and is the one
//! credential in this system a user is likely to forward by accident.
//!
//! **A note for whoever builds the HTTP side.** Do not consume a link from the
//! `GET` that renders the page. Corporate mail scanners and link-preview
//! fetchers follow every URL in every message, and a single-use `GET` is spent
//! before the human ever clicks it — the failure looks exactly like an attack
//! and is not. Render a page with a button, and consume on the `POST`.

use chrono::{DateTime, Duration, Utc};
use df_core::audit::{action, Entry};
use df_core::Db;
use serde::Serialize;

use crate::crypto::{self, prefix};
use crate::error::{AuthError, Result};
use crate::ratelimit;

/// How long a link is good for.
///
/// Ten minutes is the design's figure and it is chosen against the mailbox, not
/// the network: the link's real exposure is the hours it spends sitting in an
/// inbox that may be shared, synced to a phone, or forwarded. Ten minutes is
/// enough for a human to switch to their mail client and click, and short
/// enough that the copy left behind is inert.
pub const TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "magic_link_purpose", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Signup: prove the address belongs to whoever typed it.
    VerifyEmail,
    /// The authenticator is gone. Consuming this resets TOTP.
    RecoverTotp,
    /// Join an org you were invited to.
    AcceptInvite,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::VerifyEmail => "verify_email",
            Purpose::RecoverTotp => "recover_totp",
            Purpose::AcceptInvite => "accept_invite",
        }
    }
}

/// A minted link. The caller mails `token` and stores nothing.
pub struct Issued {
    /// Plaintext, for the URL. Handed over exactly once.
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Redacting, for the same reason as [`crate::crypto::Secret`]: this is a live
/// credential and a derived `Debug` would put it in the first log line that
/// touches it.
impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Mint a link for `email`.
///
/// Takes an address rather than a `UserId` because the earliest caller — signup
/// — has no user row yet, and because the consuming side must not learn whether
/// one exists.
pub async fn issue(db: &Db, email: &str, purpose: Purpose) -> Result<Issued> {
    let email = email.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(AuthError::InvalidRequest(format!(
            "{email:?} is not an email address"
        )));
    }
    let key = email.to_lowercase();

    // Throttle *issuance*, not just redemption. The thing being defended here
    // is somebody else's mailbox: an attacker who cannot log in can still point
    // a "send me a link" endpoint at a victim's address forever.
    let bucket = format!("magic:{}:{key}", purpose.as_str());
    ratelimit::check(db, &bucket).await?;
    ratelimit::charge(db, &bucket).await?;

    // One live link per (address, purpose). A user who clicks "resend" three
    // times because the first mail was slow should end up with one link that
    // works, not three that all do — and an old link left live is an old link
    // an attacker can still use.
    sqlx::query(
        "UPDATE magic_links SET consumed_at = now() \
         WHERE lower(email) = $1 AND purpose = $2 AND consumed_at IS NULL",
    )
    .bind(&key)
    .bind(purpose)
    .execute(db.pool())
    .await?;

    let token = crypto::generate(prefix::MAGIC);
    let expires_at = Utc::now() + Duration::minutes(TTL_MINUTES);

    sqlx::query(
        "INSERT INTO magic_links (email, purpose, token_hash, expires_at) VALUES ($1,$2,$3,$4)",
    )
    .bind(email)
    .bind(purpose)
    .bind(&token.hash)
    .bind(expires_at)
    .execute(db.pool())
    .await?;

    // Best-effort, like every other auth audit: losing a row is bad, refusing
    // to send the link because the audit table is unavailable is worse.
    if let Err(e) = db
        .audit_global(
            Entry::new(action::MAGIC_LINK_SENT)
                .actor_label(email)
                .detail(serde_json::json!({ "purpose": purpose.as_str() })),
        )
        .await
    {
        tracing::error!(error = %e, "failed to audit a magic link issuance");
    }

    Ok(Issued {
        token: token.into_plaintext(),
        expires_at,
    })
}

/// Spend a link, returning the address it was issued to.
///
/// `expected` is part of the lookup rather than a check afterwards, so a
/// `verify_email` link presented to the recovery endpoint is simply not found —
/// it is neither honoured nor burned. Cross-purpose reuse is the interesting
/// attack on a shared token table (an email-verification link is much easier to
/// obtain than a recovery one), and matching on purpose closes it without
/// costing a legitimate user their link.
pub async fn consume(db: &Db, presented: &str, expected: Purpose) -> Result<String> {
    let hash = crypto::hash(presented.trim());

    // Claim atomically. Two clicks — a prefetching mail client and then the
    // human, or two tabs — must not both succeed, and only the database can
    // arbitrate that.
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "UPDATE magic_links SET consumed_at = now() \
         WHERE token_hash = $1 AND purpose = $2 AND consumed_at IS NULL \
         RETURNING email, expires_at",
    )
    .bind(&hash)
    .bind(expected)
    .fetch_optional(db.pool())
    .await?;

    // Unknown, already spent, and wrong-purpose are one answer. Which of the
    // three it was is not something the holder of a failing token should be
    // able to determine.
    let (email, expires_at) = row.ok_or(AuthError::AlreadyConsumed)?;

    if expires_at <= Utc::now() {
        // Already marked consumed above, deliberately: an expired link is dead
        // either way, and leaving it claimable would let a race retry it.
        return Err(AuthError::Expired);
    }

    if let Err(e) = db
        .audit_global(
            Entry::new(action::MAGIC_LINK_CONSUMED)
                .actor_label(&email)
                .detail(serde_json::json!({ "purpose": expected.as_str() })),
        )
        .await
    {
        tracing::error!(error = %e, "failed to audit a magic link redemption");
    }

    Ok(email)
}

/// Drop links that can no longer be redeemed.
pub async fn sweep(db: &Db) -> Result<u64> {
    let n = sqlx::query("DELETE FROM magic_links WHERE expires_at < now() - interval '1 day'")
        .execute(db.pool())
        .await?
        .rows_affected();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored labels are the Postgres enum's labels. A mismatch here is a
    /// runtime type error on every insert, which no unit test elsewhere catches.
    #[test]
    fn purpose_labels_match_the_database_enum() {
        assert_eq!(Purpose::VerifyEmail.as_str(), "verify_email");
        assert_eq!(Purpose::RecoverTotp.as_str(), "recover_totp");
        assert_eq!(Purpose::AcceptInvite.as_str(), "accept_invite");
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let issued = Issued {
            token: "df_ml_supersecret".into(),
            expires_at: Utc::now(),
        };
        let rendered = format!("{issued:?}");
        assert!(!rendered.contains("supersecret"), "Debug leaked the token");
        assert!(rendered.contains("redacted"));
    }
}
