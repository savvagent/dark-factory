//! `df-mcp` — the Streamable HTTP MCP surface.
//!
//! This crate is the product's front door: one HTTPS endpoint that any
//! MCP-speaking coding agent can be pointed at, with OAuth 2.1 in front of it
//! and `df-core`'s tenant-pinned queue behind it.
//!
//! ```text
//!   POST /mcp
//!     └─ require_bearer ──── introspect the token, pin the org, attach the principal
//!          └─ StreamableHttpService ──── rmcp session, JSON-RPC framing
//!               └─ Factory ──── one tool call
//!                    └─ Db::begin(org) ──── df-core, RLS, commit
//! ```
//!
//! ## Three decisions worth knowing before changing anything here
//!
//! **No SQL lives in this crate.** Every statement is a `df-core` method on a
//! [`df_core::Tx`], which cannot be constructed without an org. A query written
//! here would bypass the pinning that tenant isolation's second guard depends
//! on, so there is no "just this once" version of it.
//!
//! **Nothing is client-specific.** No tool annotation, hook, or capability that
//! only one agent understands, no validation of `agentType` against a list of
//! known agents, no branch on the client's name. A feature that works in one
//! coding agent and not another does not ship, so the surface is plain
//! Streamable HTTP and plain JSON Schema.
//!
//! **The server ships no workflow.** These tools are primitives — queue,
//! dependencies, claims, leases, messages, change notification. How work is
//! specified, planned, reviewed, or measured belongs in the customer's own
//! skills calling these tools, which is what the opaque `metadata` field on a
//! job is for. When a proposed feature could live either here or in a caller's
//! skill, it belongs in the skill.

pub mod auth;
pub mod error;
pub mod server;
pub mod tools;

use std::sync::Arc;

use axum::routing::{any_service, get};
use axum::Router;
use df_core::watch::Watcher;
use df_core::Db;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

pub use auth::ResourceServer;
pub use server::Factory;

/// Deployment-dependent settings the MCP surface cannot infer for itself.
#[derive(Debug, Clone)]
pub struct Config {
    /// This resource's canonical URI, and the audience every token must carry.
    /// Must match what the authorization server mints tokens for.
    pub resource_uri: String,
    /// Public base URL, used to build the discovery pointer in a `401`.
    pub public_url: String,
    /// Hostnames or `host:port` authorities accepted in the `Host` header.
    ///
    /// `rmcp` defaults this to loopback only, which is a sensible default for
    /// the local servers it is usually used to write and completely wrong for
    /// a hosted one: leave it and every request from the internet is rejected
    /// with an error that says nothing about hostnames. It is required here so
    /// nobody discovers that in production.
    pub allowed_hosts: Vec<String>,
    /// Browser origins accepted on requests that carry `Origin`. Empty disables
    /// the check, which is right for a surface reached by CLI agents rather
    /// than by pages.
    pub allowed_origins: Vec<String>,
    /// Refuse billable calls once an org on a hard-stop plan is past its
    /// bucket. **Off by default**, and off for milestone 1: recording history
    /// is worth having long before anyone's work is refused over it, and a
    /// counter that starts rejecting calls before the numbers have been
    /// watched in anger is a support incident rather than a revenue feature.
    pub enforce_quotas: bool,
    /// Where a caller who has run out is sent. Named in the refusal itself,
    /// because an error that says "upgrade" without saying where is a dead end.
    pub upgrade_url: String,
}

impl Config {
    pub fn new(resource_uri: impl Into<String>, public_url: impl Into<String>) -> Self {
        let public_url = public_url.into();
        let host = url::Url::parse(&public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();

        Self {
            resource_uri: resource_uri.into(),
            allowed_hosts: if host.is_empty() { vec![] } else { vec![host] },
            allowed_origins: vec![],
            enforce_quotas: false,
            upgrade_url: format!("{}/settings/billing", public_url.trim_end_matches('/')),
            public_url,
        }
    }
}

/// Build the MCP surface, ready to be nested into `df-server`'s router.
///
/// Two routes, and only one of them is authenticated:
///
/// - `/.well-known/oauth-protected-resource` is deliberately **open**. It is
///   what an unauthenticated client reads to discover how to authenticate, so
///   putting it behind authentication would be a closed loop.
/// - `/mcp` requires a bearer token audienced for [`Config::resource_uri`].
///
/// The transport runs **stateless** (`stateful_mode: false`, `json_response:
/// true`). dark-factory never pushes to a client — `watch` is a long poll the
/// agent initiates — so a server-held session would buy nothing and cost the
/// one thing a hosted multi-replica deployment cannot easily give it: every
/// request from one client landing on the replica that holds its session.
/// Stateless means any replica can serve any request, with no sticky routing
/// and no shared session store.
pub fn router(db: Db, watcher: Arc<Watcher>, config: Config) -> Router {
    let rs = Arc::new(ResourceServer::new(
        db.clone(),
        config.resource_uri.clone(),
        config.public_url.clone(),
    ));

    // #[non_exhaustive], so built by mutation rather than a struct literal.
    let mut transport = StreamableHttpServerConfig::default();
    transport.stateful_mode = false;
    transport.json_response = true;
    transport.allowed_hosts = config.allowed_hosts;
    transport.allowed_origins = config.allowed_origins;

    let factory = Factory::new(
        db,
        watcher,
        df_billing::Meter::new(config.enforce_quotas, config.upgrade_url),
    );
    let service = StreamableHttpService::new(
        // Called per session. `Factory` is cheap to clone — a pool handle, an
        // `Arc`, and the tool router — so this is not a per-request cost worth
        // engineering around.
        move || Ok(factory.clone()),
        Arc::new(LocalSessionManager::default()),
        transport,
    );

    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(auth::protected_resource_metadata),
        )
        .route_service(
            "/mcp",
            any_service(service).layer(axum::middleware::from_fn_with_state(
                rs.clone(),
                auth::require_bearer,
            )),
        )
        .with_state(rs)
}
