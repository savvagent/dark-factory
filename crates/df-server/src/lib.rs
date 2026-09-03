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

use anyhow::{Context, Result};
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
///
/// Fallible because the encryption key is only a `String` until something tries
/// to use it. `Config::from_env` cannot prove the key parses without building a
/// `Cipher`, and a `Config` assembled by hand — a test, a future binary — need
/// not have come from the environment at all. Returning the error keeps this
/// crate's rule that a bad value is a named failure and never a panic.
pub fn router(db: Db, watcher: Arc<Watcher>, config: &Config) -> Result<Router> {
    let web = df_web::router(web_state(db.clone(), config)?);

    let mcp = df_mcp::mcp_endpoint(db.clone(), watcher, mcp_config(config));

    Ok(health::router(db)
        .merge(web)
        .merge(mcp)
        .fallback_service(console(config))
        // Request spans, without headers. `DefaultMakeSpan::include_headers`
        // would put `Authorization` and `Cookie` into the logs — every bearer
        // token and every session cookie, in plaintext, in whatever the log
        // aggregator retains. Do not turn it on.
        .layer(TraceLayer::new_for_http()))
}

/// `df-web`'s state, with the settings that are this deployment's to decide.
fn web_state(db: Db, config: &Config) -> Result<df_web::AppState> {
    let cipher = df_core::crypto::Cipher::from_base64_key(&config.encryption_key)
        .context("DF_ENCRYPTION_KEY is not a valid 32-byte base64 key")?;

    let web_config = web_config(config);
    // Built here and once: `rp_id` is what every passkey is bound to, so a bad
    // value must stop the process rather than surface as a browser error on
    // somebody's first sign-in.
    let webauthn = df_web::relying_party(&web_config)
        .context("could not build the WebAuthn relying party from DF_PUBLIC_URL")?;

    Ok(df_web::AppState::new(db, cipher, webauthn, web_config))
}

/// Every setting `df-web` takes from this deployment, in one place so it can be
/// tested without a database.
///
/// Split out because the failure mode is silence: a field added to
/// `df_web::Config` that nothing here assigns keeps its `Default`, and the
/// console then reports that default as fact. `enforce_quotas` reached exactly
/// that state once already — see `every_deployment_setting_reaches_df_web`.
fn web_config(config: &Config) -> df_web::Config {
    let mut web = df_web::Config::new(&config.public_url, &config.resource_uri);
    web.totp_issuer = config.totp_issuer.clone();
    web.github_app_webhook_secret = config.github_app_webhook_secret.clone();
    web.client_ip_header = config.client_ip_header.clone();
    // The console reports this as `enforced` on the usage endpoint. It has to be
    // the same value `df-mcp` is refusing calls with, or a customer whose agent
    // just got `quota_exceeded` reads their own dashboard and is told nothing is
    // being enforced.
    web.enforce_quotas = config.enforce_quotas;
    web
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

    /// A setting that `df-server` reads but never passes on is invisible: the
    /// console reports `df_web::Config`'s default and calls it fact. This is not
    /// hypothetical — `enforce_quotas` survived a merge on one side of the tree
    /// and was never assigned on the other, so an org whose agents were being
    /// refused would have been shown `enforced: false`.
    #[test]
    fn every_deployment_setting_reaches_df_web() {
        let mut config = Config::for_test();
        config.totp_issuer = "acme-factory".into();
        config.github_app_webhook_secret = Some("webhook-secret".into());
        config.client_ip_header = Some("cf-connecting-ip".into());
        config.enforce_quotas = true;

        let web = web_config(&config);

        assert_eq!(web.totp_issuer, "acme-factory");
        assert_eq!(
            web.github_app_webhook_secret.as_deref(),
            Some("webhook-secret")
        );
        assert_eq!(web.client_ip_header.as_deref(), Some("cf-connecting-ip"));
        assert!(
            web.enforce_quotas,
            "df-web was left with the default while df-mcp refuses billable calls"
        );
        assert_eq!(web.public_url, config.public_url);
        assert_eq!(web.resource_uri, config.resource_uri);
    }

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
