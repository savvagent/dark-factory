//! dark-factory server binary. Assembles every HTTP surface on one port.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use df_core::watch::Watcher;
use df_core::Db;
use df_server::{router, Config, LogFormat};
use tokio::net::TcpListener;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Loads `.env` if there is one. `dotenvy` never overwrites a variable that
    // is already set, so a file that accidentally ships inside an image cannot
    // override the deployment's real configuration.
    let dotenv = dotenvy::dotenv();

    let config = Config::from_env().context(
        "configuration is incomplete. Copy .env.example to .env for local runs, \
         or set the variables named above in the deployment",
    )?;

    init_tracing(config.log_format)?;
    match dotenv {
        Ok(path) => tracing::debug!(path = %path.display(), "loaded .env"),
        Err(_) => tracing::debug!("no .env file; using the process environment"),
    }

    // Validate the key before anything else can be built with it. `Cipher`
    // construction is infallible from here on, and a bad key is a startup
    // error naming the variable rather than a panic on the first TOTP enrolment
    // hours later.
    df_core::crypto::Cipher::from_base64_key(&config.encryption_key)
        .context("DF_ENCRYPTION_KEY is not a valid 32-byte base64 key")?;

    let db = Db::connect(&config.database_url)
        .await
        .context("could not connect to DATABASE_URL")?;

    if config.run_migrations {
        // sqlx holds a Postgres advisory lock for the whole run, so several
        // replicas starting together is safe: the losers block until the winner
        // is done rather than racing each other through the same DDL.
        tracing::info!("applying migrations");
        db.migrate().await.context("migrations failed")?;
    } else {
        tracing::warn!("DF_RUN_MIGRATIONS is off; assuming the schema is already current");
    }

    // Prove tenant isolation before binding a port, never after. Row-level
    // security is the one guard the *environment* can switch off — the same
    // migrations isolate perfectly under one database role and not at all under
    // another, and no amount of reading this repository tells you which one a
    // deployment connects as. Discovering that from a customer is not a
    // recoverable failure, so it is a startup error naming the remediation.
    let isolation = db
        .verify_tenant_isolation()
        .await
        .context("refusing to serve: tenant isolation is not enforced by this database")?;
    tracing::info!("{}", isolation.summary());

    let watcher = Watcher::spawn(db.pool().clone())
        .await
        .context("could not start the change listener")?;

    if !config.static_dir.is_dir() {
        // Loud on purpose. A missing bundle serves a
        // 404 on every console page while the API works perfectly, which looks
        // like a routing bug for as long as it takes someone to notice that
        // `npm run build` was never run.
        tracing::warn!(
            dir = %config.static_dir.display(),
            "no console bundle at DF_STATIC_DIR; the API will work and every console page will 404. \
             Run `npm run build` in web/, or set DF_STATIC_DIR"
        );
    }

    let app = router(db, watcher.clone(), &config)?;

    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("could not bind {}", config.bind))?;

    tracing::info!(
        bind = %config.bind,
        public_url = %config.public_url,
        resource_uri = %config.resource_uri,
        enforce_quotas = config.enforce_quotas,
        "df-server listening"
    );

    // `ConnectInfo` is not decoration: `df_web::state::client_ip` reads the peer
    // address out of it, and that address is what every per-IP throttle and
    // every audit entry is keyed on. Serve without this and `client_ip` returns
    // `None` for every request, which silently disables rate limiting on the
    // login and registration endpoints.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;

    // After the server, deliberately. `Watcher::spawn` takes a connection out of
    // the pool for `LISTEN` and detaches it, so dropping the pool does not
    // reclaim it; `shutdown` stops the task and waits for the connection to go.
    // Without this the process holds a Postgres session open past the point it
    // stopped serving anything.
    tracing::info!("draining");
    watcher.shutdown().await;
    tracing::info!("stopped");

    Ok(())
}

fn init_tracing(format: LogFormat) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json())
            .init(),
        LogFormat::Text => registry.with(tracing_subscriber::fmt::layer()).init(),
    }

    Ok(())
}

/// Resolves on the first `SIGTERM` or `SIGINT`.
///
/// `SIGTERM` is the one that matters — it is what a container runtime sends
/// before it waits its grace period and then sends `SIGKILL`. A server that
/// only handles `SIGINT` looks fine in a terminal and is hard-killed on every
/// single deploy, dropping whatever was in flight.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT; shutting down"),
        _ = terminate => tracing::info!("SIGTERM; shutting down"),
    }
}
