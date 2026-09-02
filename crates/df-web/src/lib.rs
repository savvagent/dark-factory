//! `df-web` — the console API and the authorization server's browser surface.
//!
//! Everything a human touches. `df-mcp` serves agents over bearer tokens;
//! this crate serves people over session cookies, and hosts the one place the
//! two meet — the OAuth consent screen, where a signed-in human grants an agent
//! a token.
//!
//! ```text
//!   Browser ──► /api/…            session cookie ──► CurrentUser / OrgCtx ──► df-core
//!           └─► /oauth/authorize  session cookie ──► consent ──► authorization code
//!   Agent   ──► /oauth/token      PKCE verifier  ──► access + refresh tokens
//! ```
//!
//! ## What holds across the whole crate
//!
//! **No SQL.** Every statement is a `df-core` method, for the same reason as in
//! `df-mcp`: a query written here would bypass the tenant-pinned transaction
//! that isolation's second guard depends on.
//!
//! **Authorization is decided by an extractor, not by a handler.**
//! [`session::OrgCtx`] resolves the caller, the org in the path, and their role
//! before a handler body runs, and a handler that needs more than membership
//! says so in one line. A handler that forgets is a handler that serves another
//! tenant's data, and a type is a better place for that than a review checklist.
//!
//! **An org you are not in is `404`.** Answering `403` on a real slug and `404`
//! on a fake one turns any signed-in account into a directory of who uses the
//! product.
//!
//! **The router and the OpenAPI document come from one list.** See
//! [`catalog`] — routes and their descriptions are the same declaration, so
//! they cannot drift apart.

pub mod catalog;
pub mod error;
pub mod mail;
pub mod oauth;
pub mod openapi;
pub mod routes;
pub mod session;
pub mod state;

use axum::Router;

pub use error::{ApiError, ApiResult};
pub use mail::{LogMailer, Mailer};
pub use state::{AppState, Config};

/// Build the console surface, ready to be merged into `df-server`'s router.
///
/// Every route comes from [`catalog::catalog`]. Grouping by path before
/// mounting is not cosmetic: `Router::route` panics when the same path is
/// registered twice, so several methods on one path have to arrive as a single
/// merged `MethodRouter`.
pub fn router(state: AppState) -> Router {
    let mut by_path: Vec<(&'static str, axum::routing::MethodRouter<AppState>)> = Vec::new();

    for endpoint in catalog::catalog() {
        match by_path.iter_mut().find(|(path, _)| *path == endpoint.path) {
            Some((_, existing)) => {
                let merged = std::mem::replace(existing, axum::routing::MethodRouter::new());
                *existing = merged.merge(endpoint.route);
            }
            None => by_path.push((endpoint.path, endpoint.route)),
        }
    }

    by_path
        .into_iter()
        .fold(Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router is built by merging method routers per path. If that merge is
    /// ever replaced with repeated `route` calls, axum panics at startup — which
    /// is a better failure than a silent one, but only if something builds the
    /// router before a deployment does.
    #[tokio::test]
    async fn the_router_assembles() {
        let db = df_core::Db::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                // Not connected — `connect_lazy` builds a pool without touching
                // the network, which is all this test needs.
                .connect_lazy("postgres://localhost/does-not-exist")
                .expect("lazy pool"),
        );

        let state = AppState::new(
            db,
            df_auth::crypto::Cipher::from_base64_key(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            )
            .expect("test key"),
            std::sync::Arc::new(LogMailer),
            Config::new("https://console.test", "https://mcp.test/mcp"),
        );

        let _router = router(state);
    }
}
