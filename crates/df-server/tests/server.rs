//! The assembled application, exercised as one router.
//!
//! Everything below is about the seams *between* crates — the places a bug can
//! only exist once `df-web`, `df-mcp`, the health routes, and the console
//! bundle share an origin. Each crate's own behaviour is tested in that crate.

use axum::body::Body;
use df_core::watch::Watcher;
use df_core::Db;
use df_server::config::LogFormat;
use df_server::Config;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

const PUBLIC: &str = "https://factory.test";
const RESOURCE: &str = "https://factory.test/mcp";

/// A 32-byte base64 key. Test material only.
const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn config(static_dir: &str) -> Config {
    Config {
        database_url: "postgres://unused".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: PUBLIC.into(),
        resource_uri: RESOURCE.into(),
        encryption_key: KEY.into(),
        totp_issuer: "dark-factory".into(),
        client_ip_header: None,
        enforce_quotas: false,
        upgrade_url: format!("{PUBLIC}/settings/billing"),
        extra_allowed_hosts: vec![],
        allowed_origins: vec![],
        static_dir: static_dir.into(),
        run_migrations: false,
        log_format: LogFormat::Text,
    }
}

/// A pool that will never connect. Port 1 refuses immediately rather than
/// hanging, so a test that wants a database failure gets one in milliseconds.
fn dead_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://localhost:1/nope")
        .expect("lazy pool")
}

/// A directory holding a stand-in `index.html`, removed on drop so a failing
/// assertion does not leave one behind.
struct Bundle(std::path::PathBuf);

impl Bundle {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("df-server-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><title>console</title>",
        )
        .unwrap();
        Self(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

async fn get(app: axum::Router, uri: &str) -> http::Response<Body> {
    app.oneshot(
        http::Request::builder()
            .uri(uri)
            .header("host", "factory.test")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn body_json(response: http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("body was not JSON")
}

async fn body_text(response: http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The regression this crate's `lib.rs` exists for.
///
/// `df-web` and `df-mcp` both serve `/.well-known/oauth-protected-resource`,
/// each for a good reason, and `axum::Router::merge` panics on the collision
/// rather than choosing. Without this test the panic is found by a deploy.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_whole_router_assembles(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();

    let _app = df_server::router(db, watcher.clone(), &config("web/build"));

    watcher.shutdown().await;
}

/// Both discovery documents answer, without a credential, on one origin. This
/// is the whole of zero-install onboarding: an agent given nothing but the MCP
/// URL reads its way from here to a token.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn discovery_is_open_and_answers_on_one_origin(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = df_server::router(db, watcher.clone(), &config("web/build"));

    let resource = get(app.clone(), "/.well-known/oauth-protected-resource").await;
    assert_eq!(resource.status(), http::StatusCode::OK);
    let doc = body_json(resource).await;
    assert_eq!(doc["resource"], RESOURCE);
    assert_eq!(doc["authorization_servers"][0], PUBLIC);

    let as_meta = get(app, "/.well-known/oauth-authorization-server").await;
    assert_eq!(as_meta.status(), http::StatusCode::OK);
    let doc = body_json(as_meta).await;
    assert_eq!(doc["issuer"], PUBLIC);

    watcher.shutdown().await;
}

/// The `401` challenge points at a document served on this same origin. It is
/// built from `DF_PUBLIC_URL`, so a deployment that gets that wrong sends every
/// client somewhere that does not answer — and nothing else would notice.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_mcp_endpoint_points_an_unauthenticated_caller_at_this_origin(pool: PgPool) {
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = df_server::router(db, watcher.clone(), &config("web/build"));

    let response = app
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "factory.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .expect("no WWW-Authenticate header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(&format!(
            r#"resource_metadata="{PUBLIC}/.well-known/oauth-protected-resource""#
        )),
        "{challenge}"
    );

    // And the document it names is reachable here, unauthenticated. A pointer
    // whose target lives on another origin is a closed loop for the client.
    let pointed_at = get(app, "/.well-known/oauth-protected-resource").await;
    assert_eq!(pointed_at.status(), http::StatusCode::OK);

    watcher.shutdown().await;
}

/// Liveness must not depend on the database, or one database blip restarts
/// every replica at once. Asked against a pool that cannot connect.
#[tokio::test]
async fn liveness_does_not_touch_the_database() {
    let app = df_server::health::router(Db::from_pool(dead_pool()));

    let response = get(app, "/healthz").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "ok");
}

/// Readiness must, or a replica that can serve nothing keeps taking traffic.
#[tokio::test]
async fn readiness_fails_when_the_database_is_unreachable() {
    let app = df_server::health::router(Db::from_pool(dead_pool()));

    let response = get(app, "/readyz").await;
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let doc = body_json(response).await;
    assert_eq!(doc["status"], "unready");
    // The reason distinguishes a refused connection from a timeout, because
    // "not ready" alone sends whoever is paged to look at the wrong thing.
    assert!(doc["reason"].is_string(), "{doc}");
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn readiness_passes_against_a_live_database(pool: PgPool) {
    let db = Db::from_pool(pool);
    let app = df_server::health::router(db);

    let response = get(app, "/readyz").await;
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "ready");
}

/// The console's `index.html` answers any path the router does not, which is
/// what makes a hard refresh of a deep link work — and must *not* answer for an
/// API path, where `200 text/html` is the shape that makes an agent retry
/// forever against a route that will never exist.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_console_fallback_stops_at_the_api(pool: PgPool) {
    let bundle = Bundle::new("spa");
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = df_server::router(db, watcher.clone(), &config(bundle.path()));

    // A console route the server has never heard of renders the app.
    let deep_link = get(app.clone(), "/o/acme/queue").await;
    assert_eq!(deep_link.status(), http::StatusCode::OK);
    assert!(body_text(deep_link)
        .await
        .contains("<title>console</title>"));

    // An API route that does not exist is a JSON 404 an agent can read.
    // Paths that match no route at all — `/api/orgs/nope` would be a `401`,
    // because it *is* a route and an unauthenticated caller is turned away
    // before anyone asks whether the org exists.
    for path in ["/api/no/such/thing", "/oauth/nope", "/.well-known/nope"] {
        let response = get(app.clone(), path).await;
        assert_eq!(
            response.status(),
            http::StatusCode::NOT_FOUND,
            "{path} should be a 404"
        );
        assert_eq!(body_json(response).await["error"], "not_found", "{path}");
    }

    // A console path that merely shares a prefix with an API one is still the
    // console: `apiary` is a legal org slug.
    let lookalike = get(app, "/apiary").await;
    assert_eq!(lookalike.status(), http::StatusCode::OK);
    assert!(body_text(lookalike)
        .await
        .contains("<title>console</title>"));

    watcher.shutdown().await;
}
