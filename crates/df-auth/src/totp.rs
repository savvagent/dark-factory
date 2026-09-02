//! Passwordless TOTP — the individual member's login.
//!
//! No password is ever set, stored, or reset. A user enrolls an authenticator
//! app once and thereafter logs in with an email address and six digits.
//!
//! Three properties make that safe, and none of them is optional:
//!
//! 1. **Replay refusal.** A ±1-step drift window means a code stays valid for
//!    ~90 seconds. Recording the consumed `(user, step)` pair means a code
//!    phished inside that window still cannot be used twice.
//! 2. **Throttling.** Six digits is a million possibilities; see [`crate::ratelimit`].
//! 3. **Recovery.** Ten single-use codes plus an emailed magic link. Without
//!    these a lost phone is a lost account, and "contact support" is not a
//!    recovery story for a product with no passwords.

use chrono::Utc;
use df_core::audit::{action, Entry};
use df_core::ids::UserId;
use df_core::Db;
use rand::RngCore;
use totp_rs::{Algorithm, Secret as TotpSecret, TOTP};

use crate::crypto::{self, Cipher};
use crate::error::{AuthError, Result};
use crate::ratelimit;

/// Seconds per TOTP step. 30 is what every authenticator app assumes.
const STEP_SECS: u64 = 30;

/// Steps of clock drift accepted on either side.
///
/// One step (±30s) is the standard compromise: it absorbs ordinary phone clock
/// skew and the seconds a user spends typing, without widening the window a
/// phisher gets to replay a code. The replay table is what makes even this
/// window safe.
const DRIFT_STEPS: i64 = 1;

const DIGITS: usize = 6;

/// How many recovery codes are issued at enrollment.
const RECOVERY_CODE_COUNT: usize = 10;

/// What a user must be shown exactly once, at enrollment.
pub struct Enrollment {
    /// `otpauth://totp/...` — render as a QR code.
    pub provisioning_uri: String,
    /// The base32 secret, for manual entry when a camera is not available.
    pub manual_key: String,
    /// Shown once and never again; only hashes are stored.
    pub recovery_codes: Vec<String>,
}

/// Begin enrollment: generate a secret, store it **unconfirmed**, and issue
/// recovery codes.
///
/// Unconfirmed matters. A credential the user has not yet proved possession of
/// cannot be used to log in, so an interrupted enrollment leaves the account
/// exactly as it was rather than half-locked behind a secret nobody scanned.
pub async fn begin_enrollment(
    db: &Db,
    cipher: &Cipher,
    user: UserId,
    email: &str,
    issuer: &str,
) -> Result<Enrollment> {
    let secret = TotpSecret::generate_secret();
    let raw = secret
        .to_bytes()
        .map_err(|_| AuthError::Crypto("failed to generate TOTP secret".into()))?;

    let totp = build_totp(&raw, email, issuer)?;
    let sealed = cipher.seal(&raw)?;

    // Replace any previous unconfirmed attempt rather than accumulating rows:
    // a user who restarts enrollment three times should end with one secret.
    sqlx::query(
        "INSERT INTO totp_credentials (user_id, secret_ct, secret_nonce, confirmed_at) \
         VALUES ($1, $2, $3, NULL) \
         ON CONFLICT (user_id) DO UPDATE \
           SET secret_ct = EXCLUDED.secret_ct, \
               secret_nonce = EXCLUDED.secret_nonce, \
               confirmed_at = NULL, \
               created_at = now()",
    )
    .bind(user)
    .bind(&sealed.ciphertext)
    .bind(&sealed.nonce)
    .execute(db.pool())
    .await?;

    let recovery_codes = issue_recovery_codes(db, user).await?;

    Ok(Enrollment {
        provisioning_uri: totp.get_url(),
        manual_key: secret.to_encoded().to_string(),
        recovery_codes,
    })
}

/// Finish enrollment by proving possession. Until this succeeds the credential
/// cannot be used to authenticate.
pub async fn confirm_enrollment(
    db: &Db,
    cipher: &Cipher,
    user: UserId,
    email: &str,
    issuer: &str,
    code: &str,
) -> Result<()> {
    let raw = load_secret(db, cipher, user, false).await?;
    let step = match_code(&raw, email, issuer, code, Utc::now().timestamp())?;
    consume_step(db, user, step).await?;

    sqlx::query("UPDATE totp_credentials SET confirmed_at = now() WHERE user_id = $1")
        .bind(user)
        .execute(db.pool())
        .await?;

    let _ = db
        .audit_global(Entry::new(action::TOTP_ENROLLED).actor(user))
        .await;

    Ok(())
}

/// Verify a login code.
///
/// Order matters: throttle first, then match, then consume. Checking the code
/// before the throttle would let an attacker keep guessing at full speed;
/// consuming the step before confirming the match would let a wrong guess burn
/// a step the legitimate user is about to need.
pub async fn verify(
    db: &Db,
    cipher: &Cipher,
    user: UserId,
    email: &str,
    issuer: &str,
    code: &str,
    ip: Option<&str>,
) -> Result<()> {
    let user_bucket = format!("totp:user:{user}");
    ratelimit::check(db, &user_bucket).await?;
    if let Some(ip) = ip {
        ratelimit::check(db, &format!("totp:ip:{ip}")).await?;
    }

    let outcome = verify_inner(db, cipher, user, email, issuer, code).await;

    let ok = outcome.is_ok();
    ratelimit::record(db, &user_bucket, ok).await?;
    if let Some(ip) = ip {
        ratelimit::record(db, &format!("totp:ip:{ip}"), ok).await?;
    }

    // Audit both outcomes. A failed-login record is the one an incident
    // responder actually wants, and it is best-effort: an audit write failure
    // must not turn into an authentication outage.
    let entry = Entry::new(if ok {
        action::LOGIN_SUCCEEDED
    } else {
        action::LOGIN_FAILED
    })
    .actor(user)
    .from_request(ip, None)
    .detail(serde_json::json!({ "method": "totp" }));
    if let Err(e) = db.audit_global(entry).await {
        tracing::error!(error = %e, "failed to write audit event for a login attempt");
    }

    outcome
}

async fn verify_inner(
    db: &Db,
    cipher: &Cipher,
    user: UserId,
    email: &str,
    issuer: &str,
    code: &str,
) -> Result<()> {
    let raw = load_secret(db, cipher, user, true).await?;
    let step = match_code(&raw, email, issuer, code, Utc::now().timestamp())?;
    consume_step(db, user, step).await
}

/// Which step a code matches, if any.
///
/// Pure and separately testable — drift and replay are the two things most
/// likely to be subtly wrong, and neither should need a database to exercise.
/// Comparison is constant-time: a byte-by-byte early exit on a six-digit code
/// is a real timing oracle when an attacker can make unlimited attempts.
fn match_code(secret: &[u8], email: &str, issuer: &str, code: &str, now: i64) -> Result<i64> {
    let code = code.trim();
    if code.len() != DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AuthError::BadTotpCode);
    }

    let totp = build_totp(secret, email, issuer)?;
    let current = now / STEP_SECS as i64;

    let mut matched: Option<i64> = None;
    for step in (current - DRIFT_STEPS)..=(current + DRIFT_STEPS) {
        let expected = totp.generate((step * STEP_SECS as i64).max(0) as u64);
        // No early break: checking every candidate regardless of an earlier
        // match keeps the work independent of which step hit.
        if crypto::verify(expected.as_bytes(), code.as_bytes()) {
            matched = Some(step);
        }
    }

    matched.ok_or(AuthError::BadTotpCode)
}

/// Record a consumed step, refusing a replay.
///
/// The unique constraint on `(user_id, step)` is the enforcement point — not an
/// application-level "have I seen this?" check, which would race two concurrent
/// submissions of the same phished code.
async fn consume_step(db: &Db, user: UserId, step: i64) -> Result<()> {
    let inserted = sqlx::query(
        "INSERT INTO totp_used_steps (user_id, step) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(user)
    .bind(step)
    .execute(db.pool())
    .await?
    .rows_affected();

    if inserted == 0 {
        return Err(AuthError::TotpReplay);
    }
    Ok(())
}

/// A row from `totp_credentials`.
#[derive(sqlx::FromRow)]
struct CredentialRow {
    secret_ct: Vec<u8>,
    secret_nonce: Vec<u8>,
    confirmed_at: Option<chrono::DateTime<Utc>>,
}

async fn load_secret(
    db: &Db,
    cipher: &Cipher,
    user: UserId,
    require_confirmed: bool,
) -> Result<Vec<u8>> {
    let row: Option<CredentialRow> = sqlx::query_as(
        "SELECT secret_ct, secret_nonce, confirmed_at FROM totp_credentials WHERE user_id = $1",
    )
    .bind(user)
    .fetch_optional(db.pool())
    .await?;

    let row = row.ok_or(AuthError::NoTotp)?;
    // An unconfirmed credential cannot authenticate: an interrupted enrollment
    // must leave the account exactly as it was, not half-locked behind a secret
    // nobody finished scanning.
    if require_confirmed && row.confirmed_at.is_none() {
        return Err(AuthError::NoTotp);
    }
    cipher.open(&row.secret_ct, &row.secret_nonce)
}

fn build_totp(secret: &[u8], email: &str, issuer: &str) -> Result<TOTP> {
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        DRIFT_STEPS as u8,
        STEP_SECS,
        secret.to_vec(),
        Some(issuer.to_string()),
        email.to_string(),
    )
    .map_err(|e| AuthError::Crypto(format!("invalid TOTP parameters: {e}")))
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Alphabet for recovery codes: Crockford base32 minus the characters people
/// misread. No I/L/O/U, so a code read off paper cannot be transcribed wrong.
const RECOVERY_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A human-transcribable code: four groups of five, ~100 bits of entropy.
fn generate_recovery_code() -> String {
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| RECOVERY_ALPHABET[(*b as usize) % RECOVERY_ALPHABET.len()] as char)
        .collect();
    chars
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Replace this user's recovery codes, returning the plaintext once.
pub async fn issue_recovery_codes(db: &Db, user: UserId) -> Result<Vec<String>> {
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = $1 AND used_at IS NULL")
        .bind(user)
        .execute(db.pool())
        .await?;

    let mut out = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let code = generate_recovery_code();
        sqlx::query("INSERT INTO recovery_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user)
            .bind(crypto::hash(&code))
            .execute(db.pool())
            .await?;
        out.push(code);
    }
    Ok(out)
}

/// Spend a recovery code. Single-use, enforced by marking the row rather than
/// deleting it, so the audit trail can show a code *was* used.
pub async fn consume_recovery_code(
    db: &Db,
    user: UserId,
    code: &str,
    ip: Option<&str>,
) -> Result<()> {
    let bucket = format!("recovery:user:{user}");
    ratelimit::check(db, &bucket).await?;

    let normalized = code.trim().to_uppercase();
    let hash = crypto::hash(&normalized);

    let updated = sqlx::query(
        "UPDATE recovery_codes SET used_at = now() \
         WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user)
    .bind(&hash)
    .execute(db.pool())
    .await?
    .rows_affected();

    let ok = updated > 0;
    ratelimit::record(db, &bucket, ok).await?;

    if !ok {
        return Err(AuthError::BadRecoveryCode);
    }

    let _ = db
        .audit_global(
            Entry::new(action::RECOVERY_CODE_USED)
                .actor(user)
                .from_request(ip, None),
        )
        .await;

    Ok(())
}

/// How many unused recovery codes remain. The console warns when this gets low
/// — a user down to their last code is one lost phone from a support ticket.
pub async fn remaining_recovery_codes(db: &Db, user: UserId) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recovery_codes WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user)
    .fetch_one(db.pool())
    .await?;
    Ok(n)
}

/// Clear consumed-step rows that can no longer be replayed. Anything older than
/// the drift window is unreachable, so retaining it only grows the table.
pub async fn sweep_used_steps(db: &Db) -> Result<u64> {
    let n = sqlx::query("DELETE FROM totp_used_steps WHERE used_at < now() - interval '1 hour'")
        .execute(db.pool())
        .await?
        .rows_affected();
    Ok(n)
}

/// Does this user have a TOTP credential they can actually log in with?
///
/// An unconfirmed credential does not count — it is an abandoned enrollment,
/// and treating it as a second factor would lock the account behind a secret
/// nobody finished scanning.
pub async fn has_confirmed_credential(db: &Db, user: UserId) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM totp_credentials WHERE user_id = $1 AND confirmed_at IS NOT NULL",
    )
    .bind(user)
    .fetch_one(db.pool())
    .await?;
    Ok(n > 0)
}

/// Remove this user's second factor entirely, so they can enrol a new one.
///
/// Called after a recovery magic link is redeemed — the premise of that flow is
/// that the authenticator is gone, so leaving the old secret in place would
/// leave the account permanently unreachable by its own login path.
///
/// The unused recovery codes go too. They were printed alongside the secret
/// being discarded, they may well be in the same lost wallet, and enrollment
/// issues a fresh set anyway.
pub async fn reset(db: &Db, user: UserId, ip: Option<&str>) -> Result<()> {
    let mut tx = db.begin_unpinned().await?;

    sqlx::query("DELETE FROM totp_credentials WHERE user_id = $1")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = $1 AND used_at IS NULL")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    // Consumed steps belong to a secret that no longer exists. Keeping them
    // would refuse a step number the *new* secret is entitled to reuse.
    sqlx::query("DELETE FROM totp_used_steps WHERE user_id = $1")
        .bind(user)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let _ = db
        .audit_global(
            Entry::new(action::TOTP_RESET)
                .actor(user)
                .from_request(ip, None),
        )
        .await;

    Ok(())
}

/// Do a verification's arithmetic for an address that has no account.
///
/// Called from [`crate::login`] on the unknown-user path. It does not make the
/// two paths take equal time — a real verification also reads a row and writes
/// one — but the modular exponentiation-free HMAC over three candidate steps is
/// the largest single term that would otherwise be missing, and skipping it
/// makes "no such user" trivially distinguishable with a stopwatch. The
/// throttle is what actually makes the residual difference unusable; this
/// narrows it rather than pretending to close it.
pub(crate) fn decoy_check(code: &str) {
    // A fixed secret is fine: the result is discarded and only the work matters.
    const DECOY: [u8; 20] = [0x5f; 20];
    let _ = std::hint::black_box(match_code(
        &DECOY,
        "decoy@invalid",
        "dark-factory",
        code,
        Utc::now().timestamp(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMAIL: &str = "rob@example.test";
    const ISSUER: &str = "dark-factory";

    fn secret() -> Vec<u8> {
        TotpSecret::generate_secret().to_bytes().unwrap()
    }

    fn code_at(secret: &[u8], t: i64) -> String {
        build_totp(secret, EMAIL, ISSUER)
            .unwrap()
            .generate(t as u64)
    }

    #[test]
    fn current_code_matches_the_current_step() {
        let s = secret();
        let now = 1_700_000_000;
        let step = match_code(&s, EMAIL, ISSUER, &code_at(&s, now), now).unwrap();
        assert_eq!(step, now / STEP_SECS as i64);
    }

    /// One step of drift in each direction, and no more. The window has to be
    /// wide enough for a slow phone and narrow enough to be worth defending.
    #[test]
    fn drift_is_accepted_to_exactly_one_step() {
        let s = secret();
        let now = 1_700_000_000;

        for delta in [-1i64, 0, 1] {
            let t = now + delta * STEP_SECS as i64;
            assert!(
                match_code(&s, EMAIL, ISSUER, &code_at(&s, t), now).is_ok(),
                "drift of {delta} step(s) should be accepted"
            );
        }

        for delta in [-2i64, 2, 5] {
            let t = now + delta * STEP_SECS as i64;
            assert!(
                match_code(&s, EMAIL, ISSUER, &code_at(&s, t), now).is_err(),
                "drift of {delta} step(s) must be refused"
            );
        }
    }

    #[test]
    fn malformed_codes_are_refused_without_computing() {
        let s = secret();
        let now = 1_700_000_000;
        for bad in ["", "12345", "1234567", "abcdef", "12 34 56", "١٢٣٤٥٦"] {
            assert!(
                match_code(&s, EMAIL, ISSUER, bad, now).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn another_users_secret_does_not_match() {
        let a = secret();
        let b = secret();
        let now = 1_700_000_000;
        assert!(match_code(&a, EMAIL, ISSUER, &code_at(&b, now), now).is_err());
    }

    #[test]
    fn recovery_codes_are_transcribable_and_distinct() {
        let codes: Vec<String> = (0..50).map(|_| generate_recovery_code()).collect();
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len(), "recovery codes collided");

        for c in &codes {
            assert_eq!(c.len(), 23, "expected 4 groups of 5 plus 3 dashes: {c}");
            for ch in c.chars().filter(|c| *c != '-') {
                assert!(
                    RECOVERY_ALPHABET.contains(&(ch as u8)),
                    "{ch} is not in the transcribable alphabet"
                );
                assert!(
                    !"ILOU".contains(ch),
                    "{ch} is easily misread and must not appear"
                );
            }
        }
    }
}
