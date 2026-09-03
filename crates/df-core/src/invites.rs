//! Org invitations.
//!
//! An invitation is a credential with an address on it. It is minted by an
//! admin, mailed to someone who may not have an account yet, and spent once by
//! whoever proves they control that address.
//!
//! **This module never sees the token.** The caller (`df-web`) generates it with
//! `df_auth::crypto` and passes only the SHA-256 hash, exactly as every other
//! credential in the product is stored. The layering is not ceremony: `df-core`
//! owns SQL and knows nothing about cryptography, and an invite token that
//! existed in two crates would eventually be compared in the wrong one.
//!
//! **Acceptance is one transaction.** Claiming the invite and granting the
//! membership happen together, so a failure between them cannot leave an invite
//! spent with nobody added — the state a support ticket is made of, since the
//! link is single-use and there is nothing left to retry with.
//!
//! **The invited address is binding.** An invite mailed to `bob@acme.com` is
//! accepted only by a session whose verified address is `bob@acme.com`. Without
//! that check, a forwarded invitation mail is a way into someone else's org for
//! whoever reads it first.

use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use crate::ids::{OrgId, UserId};
use crate::orgs::Role;
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

/// How long an invitation is good for.
///
/// Long enough to survive a holiday, short enough that a mailbox breached next
/// quarter is not a way into the org. Deliberately far longer than a magic link
/// ([`crate::super`] — `df_auth::magic::TTL_MINUTES`): a magic link is clicked
/// within minutes of being requested, while an invitation waits on a human who
/// did not ask for it.
pub const TTL_DAYS: i64 = 14;

/// An invitation as the console reads it. No token, hashed or otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    pub id: Uuid,
    pub org_id: OrgId,
    pub email: String,
    pub role: Role,
    pub invited_by: Option<UserId>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub accepted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const INVITE_COLS: &str =
    "id, org_id, email, role, invited_by, expires_at, accepted_at, created_at";

impl Tx<'_> {
    /// Mint an invitation, superseding any live one for the same address.
    ///
    /// One live invite per (org, address), for the same reason as one live magic
    /// link per (address, purpose): an admin who clicks "resend" because the
    /// first mail was slow should end up with one link that works, and an old
    /// link left live is an old link an attacker can still use.
    pub async fn create_invite(
        &mut self,
        email: &str,
        role: Role,
        invited_by: Option<UserId>,
        token_hash: &[u8],
    ) -> Result<Invite> {
        let email = email.trim();
        if email.is_empty() || !email.contains('@') {
            return Err(Error::Invalid(format!("{email:?} is not an email address")));
        }
        let org = self.org();

        // Already in the org? Say so rather than mailing an invitation that
        // would be refused on acceptance.
        let existing: Option<Role> = sqlx::query_scalar(
            "SELECT m.role FROM org_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND lower(u.email) = lower($2)",
        )
        .bind(org)
        .bind(email)
        .fetch_optional(self.conn())
        .await?;

        if let Some(role) = existing {
            return Err(Error::AlreadyAMember {
                email: email.to_string(),
                role: format!("{role:?}").to_lowercase(),
            });
        }

        sqlx::query(
            "DELETE FROM org_invites \
             WHERE org_id = $1 AND lower(email) = lower($2) AND accepted_at IS NULL",
        )
        .bind(org)
        .bind(email)
        .execute(self.conn())
        .await?;

        let invite = sqlx::query_as(&format!(
            "INSERT INTO org_invites (org_id, email, role, invited_by, token_hash, expires_at) \
             VALUES ($1,$2,$3,$4,$5, now() + make_interval(days => $6)) \
             RETURNING {INVITE_COLS}"
        ))
        .bind(org)
        .bind(email)
        .bind(role)
        .bind(invited_by)
        .bind(token_hash)
        // `days` is an integer parameter of make_interval; binding an f64 fails
        // at runtime with a no-such-function error.
        .bind(TTL_DAYS as i32)
        .fetch_one(self.conn())
        .await?;

        Ok(invite)
    }

    /// Invitations still worth showing: not yet accepted, not yet expired.
    pub async fn list_invites(&mut self) -> Result<Vec<Invite>> {
        let org = self.org();
        let rows = sqlx::query_as(&format!(
            "SELECT {INVITE_COLS} FROM org_invites \
             WHERE org_id = $1 AND accepted_at IS NULL AND expires_at > now() \
             ORDER BY created_at DESC"
        ))
        .bind(org)
        .fetch_all(self.conn())
        .await?;
        Ok(rows)
    }

    /// Withdraw an invitation. Deleted rather than marked, because an invite
    /// that was never accepted has nothing worth keeping and a stale row is one
    /// more thing that could be resurrected.
    pub async fn revoke_invite(&mut self, id: Uuid) -> Result<()> {
        let org = self.org();
        let n = sqlx::query("DELETE FROM org_invites WHERE org_id = $1 AND id = $2")
            .bind(org)
            .bind(id)
            .execute(self.conn())
            .await?
            .rows_affected();

        if n == 0 {
            return Err(Error::InviteInvalid);
        }
        Ok(())
    }

    /// Read an invitation by its token hash without spending it — what the
    /// "you have been invited to Acme" page renders before the user decides.
    pub async fn peek_invite(&mut self, token_hash: &[u8]) -> Result<Invite> {
        let org = self.org();
        sqlx::query_as(&format!(
            "SELECT {INVITE_COLS} FROM org_invites \
             WHERE org_id = $1 AND token_hash = $2 AND accepted_at IS NULL AND expires_at > now()"
        ))
        .bind(org)
        .bind(token_hash)
        .fetch_optional(self.conn())
        .await?
        .ok_or(Error::InviteInvalid)
    }

    /// Spend an invitation and grant the membership, atomically.
    ///
    /// `user_email` is the *verified* address of the signed-in user, and it must
    /// match the address the invitation was sent to. The mismatch gets its own
    /// error naming both addresses: unlike a login failure there is no
    /// enumeration to defend against — the caller already holds a token that was
    /// mailed to that address — and "sign in as bob@acme.com to accept this"
    /// is the only thing that unsticks them.
    pub async fn accept_invite(
        &mut self,
        token_hash: &[u8],
        user: UserId,
        user_email: &str,
    ) -> Result<Role> {
        let org = self.org();

        // Claim atomically. Two clicks — a prefetching mail client and then the
        // human — must not both succeed, and only the database can arbitrate.
        let claimed: Option<(String, Role)> = sqlx::query_as(
            "UPDATE org_invites SET accepted_at = now() \
             WHERE org_id = $1 AND token_hash = $2 AND accepted_at IS NULL AND expires_at > now() \
             RETURNING email, role",
        )
        .bind(org)
        .bind(token_hash)
        .fetch_optional(self.conn())
        .await?;

        // Unknown, already accepted, and expired are one answer for the same
        // reason as `magic::consume`: which of the three it was is not something
        // the holder of a failing token gets to determine.
        let (email, role) = claimed.ok_or(Error::InviteInvalid)?;

        if !email.eq_ignore_ascii_case(user_email.trim()) {
            // Roll the claim back by failing the transaction — the caller
            // discards the `Tx`, so the invitation survives for the right
            // person to accept. This is why acceptance is one transaction.
            return Err(Error::InviteWrongAccount {
                invited: email,
                signed_in_as: user_email.trim().to_string(),
            });
        }

        sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1,$2,$3) \
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(org)
        .bind(user)
        .bind(role)
        .execute(self.conn())
        .await?;

        Ok(role)
    }

    /// Drop invitations nobody can accept any more.
    pub async fn sweep_invites(&mut self) -> Result<u64> {
        let org = self.org();
        let n = sqlx::query(
            "DELETE FROM org_invites \
             WHERE org_id = $1 AND accepted_at IS NULL AND expires_at < now() - interval '30 days'",
        )
        .bind(org)
        .execute(self.conn())
        .await?
        .rows_affected();
        Ok(n)
    }
}

// ---------------------------------------------------------------- claim codes

impl Db {
    /// Issue a one-time code that lets an account register a passkey again.
    ///
    /// Minted whenever an admin clears somebody's authenticators, in the same
    /// operation. An account with no passkeys and no outstanding claim is
    /// claimable by whoever reaches registration first — the code is what makes
    /// the account re-registrable only by whoever the admin hands it to.
    ///
    /// Supersedes any live claim for the same account, for the same reason one
    /// live invite per address: an admin who resets twice because the first
    /// code went astray should end up with one code that works.
    pub async fn create_account_claim(
        &self,
        user: UserId,
        token_hash: &[u8],
        issued_by: Option<UserId>,
    ) -> Result<()> {
        let mut tx = self.begin_unpinned().await?;

        sqlx::query("DELETE FROM account_claims WHERE user_id = $1 AND consumed_at IS NULL")
            .bind(user)
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "INSERT INTO account_claims (user_id, token_hash, issued_by, expires_at) \
             VALUES ($1, $2, $3, now() + make_interval(days => 14))",
        )
        .bind(user)
        .bind(token_hash)
        .bind(issued_by)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Read a claim without spending it, to start a ceremony against it.
    ///
    /// Deliberately non-consuming: a ceremony that is interrupted between the
    /// challenge and the signature must not burn somebody's only way back into
    /// their account.
    pub async fn peek_account_claim(&self, token_hash: &[u8]) -> Result<UserId> {
        let user: Option<UserId> = sqlx::query_scalar(
            "SELECT user_id FROM account_claims \
             WHERE token_hash = $1 AND consumed_at IS NULL AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await?;

        user.ok_or(Error::InviteInvalid)
    }

    /// Spend a claim. One statement, so two requests racing the same code
    /// cannot both win.
    pub async fn consume_account_claim(&self, token_hash: &[u8]) -> Result<UserId> {
        let user: Option<UserId> = sqlx::query_scalar(
            "UPDATE account_claims SET consumed_at = now() \
             WHERE token_hash = $1 AND consumed_at IS NULL AND expires_at > now() \
             RETURNING user_id",
        )
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await?;

        user.ok_or(Error::InviteInvalid)
    }
}
