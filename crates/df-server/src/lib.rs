//! `df-server` — one binary, one port, every surface.
//!
//! The crates are a compile-time layering discipline, not separate services, so
//! this is where they stop being libraries:
//!
//! ```text
//!   /healthz /readyz          health      no database on the liveness path
//!   /api/…  /oauth/…          df-web      session cookies, the console, the AS
//!   /.well-known/…            df-web      discovery, open by necessity
//!   /mcp                      df-mcp      bearer tokens, the agent surface
//!   everything else           web/build   the console SPA, index.html fallback
//! ```
//!
//! Assembly is a library function rather than something buried in `main` so a
//! test can build the whole router. Two of the failures here are startup
//! panics — axum panics on a route registered twice, and the two crates
//! genuinely do both want to serve `/.well-known/oauth-protected-resource` —
//! and a panic at startup is only a good failure if something other than a
//! deployment reaches it first.

pub mod config;
pub mod health;

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use df_core::watch::Watcher;
use df_core::Db;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

pub use config::{Config, LogFormat};

/// Path prefixes that belong to an API rather than to the console's routing.
///
/// Everything else falls through to the single-page app, which is what makes a
/// hard refresh of `/o/acme/queue` work. An unmatched path under one of these
/// must not: a client that `GET`s `/api/orgs/nope` needs a `404` it can parse,
/// and `200 text/html` is the shape that makes an agent retry forever against a
/// route that will never exist.
const API_PREFIXES: [&str; 4] = ["/api", "/oauth", "/mcp", "/.well-known"];

fn is_api_path(path: &str) -> bool {
    API_PREFIXES
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

/// Build the whole application.
pub fn router(db: Db, watcher: Arc<Watcher>, config: &Config) -> Router {
    let web = df_web::router(web_state(db.clone(), config));

    let mcp = df_mcp::mcp_endpoint(db.clone(), watcher, mcp_config(config));

    health::router(db)
        .merge(web)
        .merge(mcp)
        .fallback_service(console(config))
        // Request spans, without headers. `DefaultMakeSpan::include_headers`
        // would put `Authorization` and `Cookie` into the logs — every bearer
        // token and every session cookie, in plaintext, in whatever the log
        // aggregator retains. Do not turn it on.
        .layer(TraceLayer::new_for_http())
}

/// `df-web`'s state, with the settings that are this deployment's to decide.
fn web_state(db: Db, config: &Config) -> df_web::AppState {
    let mut web_config = df_web::Config::new(&config.public_url, &config.resource_uri);
    web_config.totp_issuer = config.totp_issuer.clone();
    web_config.client_ip_header = config.client_ip_header.clone();

    df_web::AppState::new(
        db,
        df_auth::crypto::Cipher::from_base64_key(&config.encryption_key)
            .expect("DF_ENCRYPTION_KEY was validated at startup"),
        Arc::new(df_web::LogMailer),
        web_config,
    )
}

fn mcp_config(config: &Config) -> df_mcp::Config {
    let mut mcp = df_mcp::Config::new(&config.resource_uri, &config.public_url);
    mcp.allowed_hosts = config.allowed_hosts();
    mcp.allowed_origins = config.allowed_origins.clone();
    mcp.enforce_quotas = config.enforce_quotas;
    mcp.upgrade_url = config.upgrade_url.clone();
    mcp
}

/// The console bundle, or a JSON `404` for anything API-shaped.
///
/// `ServeDir` falls back to `index.html` for any path it has no file for, which
/// is what `adapter-static` produces and what client-side routing needs. That
/// fallback is exactly why the API prefixes are checked first.
fn console(
    config: &Config,
) -> impl tower::Service<Request<Body>, Response = Response, Error = Infallible, Future = impl Send>
       + Clone
       + Send
       + 'static {
    let assets = ServeDir::new(&config.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(config.static_dir.join("index.html")));

    tower::service_fn(move |req: Request<Body>| {
        let assets = assets.clone();
        async move {
            if is_api_path(req.uri().path()) {
                return Ok(not_found(req.uri().path()));
            }
            Ok(assets
                .oneshot(req)
                .await
                .map(|res| res.map(Body::new))
                .into_response())
        }
    })
}

fn not_found(path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not_found",
            "error_description":
                format!("no route serves {path}. See /api/openapi.json for the console API, \
                         and /.well-known/oauth-protected-resource for the MCP endpoint."),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_prefixes_do_not_match_by_string_prefix_alone() {
        assert!(is_api_path("/api"));
        assert!(is_api_path("/api/orgs/acme"));
        assert!(is_api_path("/mcp"));
        assert!(is_api_path("/.well-known/oauth-authorization-server"));

        // An org slug or a page name that merely starts with the same letters
        // belongs to the console. `/apiary` is a legal org route and must not
        // answer a JSON 404 that the SPA never gets to render.
        assert!(!is_api_path("/apiary"));
        assert!(!is_api_path("/mcp-guide"));
        assert!(!is_api_path("/o/acme/queue"));
        assert!(!is_api_path("/"));
    }
}
