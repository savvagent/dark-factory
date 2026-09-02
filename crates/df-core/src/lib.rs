//! `df-core` — the dark-factory domain: orgs, repos, jobs, leases, messages.
//!
//! This crate owns every SQL statement in the product and knows nothing about
//! HTTP, MCP, or authentication. Two rules hold throughout, and both exist to
//! make cross-tenant leakage structurally impossible rather than merely unlikely:
//!
//! 1. **Every tenant-scoped operation takes an [`OrgId`]** — usually by being a
//!    method on [`Tx`], which cannot be constructed without one. There is no
//!    function in this crate that reads a job without naming an org.
//! 2. **Every tenant transaction runs pinned.** [`Db::begin`] issues
//!    `SET LOCAL ROLE df_app` and `SET LOCAL app.org_id`, so Postgres row-level
//!    security applies even when the connecting user owns the tables. A query
//!    that forgets its `org_id` predicate returns nothing instead of leaking.

pub mod audit;
pub mod db;
pub mod error;
pub mod ids;
pub mod invites;
pub mod jobs;
pub mod leases;
pub mod messages;
pub mod orgs;
pub mod repos;
pub mod teams;
pub mod usage;
pub mod watch;

pub use db::{Db, Tx};
pub use error::{Error, Result};
pub use ids::{JobId, OrgId, RepoId, TeamId, UserId};
