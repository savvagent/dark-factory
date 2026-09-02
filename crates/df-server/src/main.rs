//! dark-factory server binary. Assembles every HTTP surface on one port.
//!
//! `df-mcp` (agents, bearer tokens) and `df-web` (humans, session cookies) are
//! separate crates for compile-time layering, not separate processes — see
//! CLAUDE.md. This binary is the only place they are merged into one router
//! and bound to one address.
//!
//! Startup order matters: migrations must finish before the watcher starts
//! listening (a fresh database has no `df_changes` channel to listen on until
//! the migration that creates its trigger runs), and the watcher must be
//! running before either router is built, because `df-mcp`'s `watch` tool
//! holds a reference to it.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use df_core::watch::Watcher;
use df_core::Db;
use tower_http::trace::TraceLayer;

/// Read a required environment variable, or fail with a message that names it.
fn require_env(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .map_err(|_| anyhow::anyhow!("{name} is not set; see .env.example for the full list"))
}

/// Read an optional environment variable, falling back to `default` when unset
/// or empty — several of these are documented as "blank means derive a
/// default" in `.env.example`, so an empty string must not shadow it.
fn env_or(name: &str, default: impl Into<String>) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.into(),
    }
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true"))
}

/// Liveness: this process is up and can answer HTTP. No database check — a
/// database outage should surface on `/readyz`, not take the process out of
/// rotation entirely.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: this replica can currently serve tenant data. Fly's health check
/// hits this, and a failing check pulls the machine out of rotation without
/// killing it, which is the right response to a database blip.
async fn readyz(
    axum::extract::State(db): axum::extract::State<Db>,
) -> impl axum::response::IntoResponse {
    match sqlx::query("SELECT 1").execute(db.pool()).await {
        Ok(_) => (axum::http::StatusCode::OK, "ok"),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "database unreachable",
        ),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let database_url = require_env("DATABASE_URL")?;
    let bind = env_or("DF_BIND", "0.0.0.0:8080");
    let public_url = require_env("DF_PUBLIC_URL")?;
    let resource_uri = require_env("DF_RESOURCE_URI")?;
    let encryption_key = require_env("DF_ENCRYPTION_KEY")?;
    let enforce_quotas = env_flag("DF_ENFORCE_QUOTAS");
    let upgrade_url = env_or(
        "DF_UPGRADE_URL",
        format!("{}/settings/billing", public_url.trim_end_matches('/')),
    );

    let db = Db::connect(&database_url).await?;

    // Runs under a Postgres advisory lock (see `Db::migrate`), so several
    // machines starting concurrently on a fresh database wait rather than
    // racing each other through the same DDL.
    tracing::info!("running migrations");
    db.migrate().await?;

    let watcher = Watcher::spawn(db.pool().clone()).await?;

    let mut mcp_config = df_mcp::Config::new(resource_uri.clone(), public_url.clone());
    mcp_config.enforce_quotas = enforce_quotas;
    mcp_config.upgrade_url = upgrade_url.clone();
    // Not `df_mcp::router`: that includes its own copy of
    // `/.well-known/oauth-protected-resource`, and `df-web`'s router below
    // already serves that path — see `df_mcp::mcp_only_router`'s doc comment.
    let mcp_router = df_mcp::mcp_only_router(db.clone(), watcher.clone(), mcp_config);

    let cipher = df_auth::crypto::Cipher::from_base64_key(&encryption_key)?;
    // No SMTP integration exists yet (milestone 1 scope). `LogMailer` writes
    // every email to the log instead of silently dropping it — a loud no-op
    // beats a quiet one, which looks identical to a working mailer until
    // someone asks why nobody has joined. Wiring a real transport is tracked
    // separately from this task.
    let mailer: Arc<dyn df_web::Mailer> = Arc::new(df_web::LogMailer);
    let web_config = df_web::Config::new(public_url.clone(), resource_uri.clone());
    let web_state = df_web::AppState::new(db.clone(), cipher, mailer, web_config);
    let web_router = df_web::router(web_state);

    let health_router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(db.clone());

    let app = Router::new()
        .merge(mcp_router)
        .merge(web_router)
        .merge(health_router)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("DF_BIND {bind:?} is not a valid address: {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // The watcher holds a detached `LISTEN` connection that outlives the pool
    // (see CLAUDE.md's "trap in tests" note) — release it explicitly rather
    // than relying on `Drop`'s best-effort cleanup during shutdown.
    watcher.shutdown().await;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
