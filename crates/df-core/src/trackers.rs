//! Tracker connections and repo bindings.
//!
//! A connection is org-scoped provider access; a binding is the per-repo map to
//! an external tracker project or repository. Both are tenant data, so every
//! query carries an explicit `org_id = $1` predicate even though row-level
//! security would also scope it — bound from `tx.org()`, the same pinned org
//! `repos.rs` and `jobs.rs` use, so there is exactly one source of truth for
//! which org a call runs as rather than a caller-supplied value that could
//! drift from it.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::str::FromStr;

use crate::crypto::Sealed;
use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use crate::ids::{OrgId, RepoId};

const NONCE_BYTES: usize = 12;
/// AES-256-GCM's authentication tag, which the `aes-gcm` crate appends to the
/// ciphertext rather than returning separately. A combined blob shorter than
/// `NONCE_BYTES + GCM_TAG_BYTES` cannot possibly be a real sealed value —
/// there is no tag to authenticate against — so it is rejected while decoding
/// the stored encoding, before any attempt to open it.
const GCM_TAG_BYTES: usize = 16;
const CONNECTION_COLS: &str = "id, org_id, provider, external_id, encrypted_credentials, \
                               encrypted_webhook_secret, created_at, updated_at";
const BINDING_COLS: &str = "id, org_id, repo_id, connection_id, provider, external_ref, \
                            created_at, updated_at";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, schemars::JsonSchema,
)]
#[sqlx(type_name = "tracker_provider", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Jira,
}

impl std::fmt::Display for Provider {
    /// Matches the wire casing (`lowercase`, per the `serde`/`sqlx` attributes
    /// above) rather than `{:?}`'s `Github`/`Jira`, so an error message names
    /// the provider the same way the API and the database do.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provider::Github => write!(f, "github"),
            Provider::Jira => write!(f, "jira"),
        }
    }
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "github" => Ok(Self::Github),
            "jira" => Ok(Self::Jira),
            other => Err(Error::Invalid(format!(
                "unknown tracker provider {other:?}; valid providers are \"github\" and \"jira\""
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrackerConnection {
    pub id: uuid::Uuid,
    pub org_id: OrgId,
    pub provider: Provider,
    pub external_id: String,
    pub encrypted_credentials: Option<String>,
    pub encrypted_webhook_secret: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrackerBinding {
    pub id: uuid::Uuid,
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub connection_id: Option<uuid::Uuid>,
    pub provider: Provider,
    pub external_ref: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow)]
struct TrackerConnectionRow {
    id: uuid::Uuid,
    org_id: OrgId,
    provider: Provider,
    external_id: String,
    encrypted_credentials: Option<String>,
    encrypted_webhook_secret: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

fn encode_sealed(sealed: &Sealed) -> Result<String> {
    if sealed.nonce.len() != NONCE_BYTES {
        return Err(Error::Crypto("stored nonce has the wrong length".into()));
    }
    let mut combined = Vec::with_capacity(sealed.nonce.len() + sealed.ciphertext.len());
    combined.extend_from_slice(&sealed.nonce);
    combined.extend_from_slice(&sealed.ciphertext);
    Ok(B64.encode(combined))
}

/// Decode a tracker secret stored as `base64(nonce || ciphertext)`.
///
/// `tracker_connections` keeps recoverable secrets in one text column rather
/// than two small binary columns; callers that need to open one use this to
/// rebuild the storage-agnostic [`Sealed`] shape `Cipher` works with.
pub fn decode_stored_secret(encoded: &str) -> Result<Sealed> {
    // A base64 or length failure here is a corrupted *stored encoding*, not a
    // failed decryption — `Cipher::open` hasn't been called yet, so the
    // caller has not learned anything about the key or the ciphertext's
    // authenticity. Say so distinctly; conflating the two would send an
    // operator debugging a truncated column value chasing the wrong key.
    let combined = B64
        .decode(encoded)
        .map_err(|_| Error::Crypto("stored sealed value is not valid base64".into()))?;
    if combined.len() < NONCE_BYTES + GCM_TAG_BYTES {
        return Err(Error::Crypto(
            "stored sealed value is too short to contain a nonce and an authentication tag".into(),
        ));
    }
    Ok(Sealed {
        nonce: combined[..NONCE_BYTES].to_vec(),
        ciphertext: combined[NONCE_BYTES..].to_vec(),
    })
}

fn validate_connection(row: TrackerConnectionRow) -> Result<TrackerConnection> {
    row.encrypted_credentials
        .as_deref()
        .map(decode_stored_secret)
        .transpose()?;
    row.encrypted_webhook_secret
        .as_deref()
        .map(decode_stored_secret)
        .transpose()?;

    Ok(TrackerConnection {
        id: row.id,
        org_id: row.org_id,
        provider: row.provider,
        external_id: row.external_id,
        encrypted_credentials: row.encrypted_credentials,
        encrypted_webhook_secret: row.encrypted_webhook_secret,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

async fn require_repo_in_org(tx: &mut Tx<'_>, repo_id: RepoId) -> Result<()> {
    if tx.get_repo(repo_id).await?.is_some() {
        return Ok(());
    }
    Err(Error::RepoNotFound(repo_id.to_string()))
}

async fn require_connection_in_org(
    tx: &mut Tx<'_>,
    connection_id: uuid::Uuid,
    provider: Provider,
) -> Result<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tracker_connections \
         WHERE org_id = $1 AND id = $2 AND provider = $3)",
    )
    .bind(tx.org())
    .bind(connection_id)
    .bind(provider)
    .fetch_one(tx.conn())
    .await?;

    if exists {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "tracker connection {connection_id} not found in this org for provider {provider}"
        )))
    }
}

async fn upsert_connection_index(tx: &mut Tx<'_>, connection: &TrackerConnection) -> Result<()> {
    sqlx::query(
        "DELETE FROM tracker_connection_index \
         WHERE connection_id = $1 AND external_id <> $2",
    )
    .bind(connection.id)
    .bind(&connection.external_id)
    .execute(tx.conn())
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO tracker_connection_index (provider, external_id, org_id, connection_id) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (provider, external_id) DO NOTHING",
    )
    .bind(connection.provider)
    .bind(&connection.external_id)
    .bind(connection.org_id)
    .bind(connection.id)
    .execute(tx.conn())
    .await?
    .rows_affected();

    if inserted == 0 {
        let existing: (OrgId, uuid::Uuid) = sqlx::query_as(
            "SELECT org_id, connection_id FROM tracker_connection_index \
             WHERE provider = $1 AND external_id = $2",
        )
        .bind(connection.provider)
        .bind(&connection.external_id)
        .fetch_one(tx.conn())
        .await?;

        if existing.0 != connection.org_id || existing.1 != connection.id {
            return Err(Error::Invalid(format!(
                "tracker connection {provider} external id {external_id:?} is already registered to another org",
                provider = connection.provider,
                external_id = connection.external_id,
            )));
        }
    }

    Ok(())
}

pub async fn upsert_connection(
    tx: &mut Tx<'_>,
    provider: Provider,
    external_id: &str,
    encrypted_credentials: Option<&Sealed>,
    encrypted_webhook_secret: Option<&Sealed>,
) -> Result<TrackerConnection> {
    let org_id = tx.org();
    let encrypted_credentials = encrypted_credentials.map(encode_sealed).transpose()?;
    let encrypted_webhook_secret = encrypted_webhook_secret.map(encode_sealed).transpose()?;

    let row: TrackerConnectionRow = sqlx::query_as(&format!(
        "INSERT INTO tracker_connections \
         (org_id, provider, external_id, encrypted_credentials, encrypted_webhook_secret) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id, provider) DO UPDATE SET \
           external_id = EXCLUDED.external_id, \
           encrypted_credentials = EXCLUDED.encrypted_credentials, \
           encrypted_webhook_secret = EXCLUDED.encrypted_webhook_secret, \
           updated_at = now() \
         RETURNING {CONNECTION_COLS}"
    ))
    .bind(org_id)
    .bind(provider)
    .bind(external_id)
    .bind(encrypted_credentials)
    .bind(encrypted_webhook_secret)
    .fetch_one(tx.conn())
    .await?;

    let connection = validate_connection(row)?;
    upsert_connection_index(tx, &connection).await?;
    Ok(connection)
}

pub async fn get_connection(
    tx: &mut Tx<'_>,
    provider: Provider,
) -> Result<Option<TrackerConnection>> {
    let row: Option<TrackerConnectionRow> = sqlx::query_as(&format!(
        "SELECT {CONNECTION_COLS} FROM tracker_connections WHERE org_id = $1 AND provider = $2"
    ))
    .bind(tx.org())
    .bind(provider)
    .fetch_optional(tx.conn())
    .await?;

    row.map(validate_connection).transpose()
}

pub async fn delete_connection(tx: &mut Tx<'_>, provider: Provider) -> Result<()> {
    sqlx::query("DELETE FROM tracker_connection_index WHERE org_id = $1 AND provider = $2")
        .bind(tx.org())
        .bind(provider)
        .execute(tx.conn())
        .await?;
    sqlx::query("DELETE FROM tracker_connections WHERE org_id = $1 AND provider = $2")
        .bind(tx.org())
        .bind(provider)
        .execute(tx.conn())
        .await?;
    Ok(())
}

/// Resolve which org owns a provider connection, from the provider's own
/// identifier alone.
///
/// This is the one place a tracker lookup runs before an [`OrgId`] is known —
/// analogous to `df_auth::tokens::introspect` resolving a principal before a
/// session exists. It reads only `tracker_connection_index`, which deliberately
/// holds no secrets and is outside RLS for this bootstrap hop. Every read of
/// `tracker_connections` itself still goes through an org-pinned [`Tx`]. Do
/// not add a second unscoped tracker accessor without updating spec §5a.
pub async fn resolve_connection_org(
    db: &Db,
    provider: Provider,
    external_id: &str,
) -> Result<Option<OrgId>> {
    let org_id = sqlx::query_scalar(
        "SELECT org_id FROM tracker_connection_index WHERE provider = $1 AND external_id = $2",
    )
    .bind(provider)
    .bind(external_id)
    .fetch_optional(db.pool())
    .await?;
    Ok(org_id)
}

pub async fn upsert_binding(
    tx: &mut Tx<'_>,
    repo_id: RepoId,
    connection_id: Option<uuid::Uuid>,
    provider: Provider,
    external_ref: &str,
) -> Result<TrackerBinding> {
    let org_id = tx.org();
    require_repo_in_org(tx, repo_id).await?;
    if let Some(connection_id) = connection_id {
        require_connection_in_org(tx, connection_id, provider).await?;
    }

    let binding = sqlx::query_as(&format!(
        "INSERT INTO tracker_bindings (org_id, repo_id, connection_id, provider, external_ref) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (repo_id, provider) DO UPDATE SET \
           connection_id = EXCLUDED.connection_id, \
           external_ref = EXCLUDED.external_ref, \
           updated_at = now() \
         RETURNING {BINDING_COLS}"
    ))
    .bind(org_id)
    .bind(repo_id)
    .bind(connection_id)
    .bind(provider)
    .bind(external_ref)
    .fetch_one(tx.conn())
    .await?;

    Ok(binding)
}

pub async fn get_binding(
    tx: &mut Tx<'_>,
    binding_id: uuid::Uuid,
) -> Result<Option<TrackerBinding>> {
    let binding = sqlx::query_as(&format!(
        "SELECT {BINDING_COLS} FROM tracker_bindings WHERE org_id = $1 AND id = $2"
    ))
    .bind(tx.org())
    .bind(binding_id)
    .fetch_optional(tx.conn())
    .await?;
    Ok(binding)
}

pub async fn delete_binding(tx: &mut Tx<'_>, binding_id: uuid::Uuid) -> Result<()> {
    sqlx::query("DELETE FROM tracker_bindings WHERE org_id = $1 AND id = $2")
        .bind(tx.org())
        .bind(binding_id)
        .execute(tx.conn())
        .await?;
    Ok(())
}

pub async fn resolve_binding(
    tx: &mut Tx<'_>,
    repo_id: RepoId,
    provider: Provider,
) -> Result<Option<TrackerBinding>> {
    let binding = sqlx::query_as(&format!(
        "SELECT {BINDING_COLS} FROM tracker_bindings \
         WHERE org_id = $1 AND repo_id = $2 AND provider = $3"
    ))
    .bind(tx.org())
    .bind(repo_id)
    .bind(provider)
    .fetch_optional(tx.conn())
    .await?;
    Ok(binding)
}

pub async fn find_binding_by_external_ref(
    tx: &mut Tx<'_>,
    provider: Provider,
    external_ref: &str,
) -> Result<Option<TrackerBinding>> {
    let binding = sqlx::query_as(&format!(
        "SELECT {BINDING_COLS} FROM tracker_bindings \
         WHERE org_id = $1 AND provider = $2 AND external_ref = $3"
    ))
    .bind(tx.org())
    .bind(provider)
    .bind(external_ref)
    .fetch_optional(tx.conn())
    .await?;
    Ok(binding)
}
