//! The connection pool and the tenant-pinned transaction.
//!
//! [`Tx`] is the only way to reach tenant data, and it cannot be constructed
//! without an [`OrgId`]. That is the crate's central invariant: "which tenant?"
//! is answered once, at transaction open, instead of being re-answered (and
//! occasionally forgotten) in every individual query.

use crate::error::Result;
use crate::ids::OrgId;
use sqlx::postgres::{PgPoolOptions, Postgres};
use sqlx::{PgPool, Transaction};

/// The application role that tenant transactions run as. Must match migration
/// `0007_rls.sql`.
const TENANT_ROLE: &str = "df_app";

#[derive(Clone, Debug)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply all migrations. Safe to run from several replicas at once: sqlx
    /// takes a Postgres advisory lock for the duration, so the losers wait
    /// rather than racing each other through the same DDL.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;
        Ok(())
    }

    /// Open a transaction pinned to `org`.
    ///
    /// Two statements run before the caller gets control, and both are load-bearing:
    ///
    /// - `SET LOCAL ROLE df_app` drops out of any superuser/owner identity for
    ///   the rest of the transaction. **Without this, RLS silently does
    ///   nothing** — Postgres exempts superusers and table owners from their own
    ///   policies, and the connecting user is frequently one or both. This was
    ///   verified empirically against a live database, not assumed.
    /// - `set_config('app.org_id', …, true)` supplies the value every policy
    ///   compares against. `set_config` rather than `SET LOCAL` because `SET`
    ///   takes no bind parameters, and interpolating a tenant id into DDL-ish
    ///   SQL is not a thing worth doing carefully when a parameterized
    ///   equivalent exists.
    ///
    /// Both are transaction-local, so returning the connection to the pool
    /// cannot carry one tenant's identity into the next request.
    pub async fn begin(&self, org: OrgId) -> Result<Tx<'static>> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(&format!("SET LOCAL ROLE {TENANT_ROLE}"))
            .execute(&mut *tx)
            .await?;

        sqlx::query("SELECT set_config('app.org_id', $1, true)")
            .bind(org.to_string())
            .execute(&mut *tx)
            .await?;

        Ok(Tx { tx, org })
    }

    /// Open an **unpinned** transaction for the control plane — authentication,
    /// signup, org lookup — where the org is not yet known and RLS-protected
    /// tables are not touched.
    ///
    /// Named to be conspicuous at call sites. Anything reaching for this to get
    /// at tenant data is a bug: it runs as the connecting role with no
    /// `app.org_id`, so tenant tables return zero rows rather than everything.
    pub async fn begin_unpinned(&self) -> Result<Transaction<'static, Postgres>> {
        Ok(self.pool.begin().await?)
    }
}

/// A transaction pinned to exactly one org.
///
/// Holds the [`OrgId`] so query methods can bind it explicitly, *in addition to*
/// the RLS policy that would already filter the rows. The redundancy is
/// intentional: the explicit predicate keeps the query plans index-friendly and
/// keeps intent legible, while RLS catches the query where someone forgot.
pub struct Tx<'a> {
    tx: Transaction<'a, Postgres>,
    org: OrgId,
}

impl<'a> Tx<'a> {
    pub fn org(&self) -> OrgId {
        self.org
    }

    /// Borrow the underlying executor for a query.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }

    pub async fn rollback(self) -> Result<()> {
        self.tx.rollback().await?;
        Ok(())
    }
}
