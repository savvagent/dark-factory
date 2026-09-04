mod common;

use axum::body::Body;
use common::{cipher, Harness, PUBLIC_URL, RESOURCE};
use df_core::jobs::Tracker;
use df_core::orgs::Role;
use df_core::repos::NewRepo;
use df_core::trackers::{upsert_binding, upsert_connection, Provider};
use df_web::{AppState, Config};
use hmac::{Hmac, Mac};
use http::{Request, StatusCode};
use sha2::Sha256;
use sqlx::PgPool;
use tower::ServiceExt;

type HmacSha256 = Hmac<Sha256>;

const GITHUB_SECRET: &str = "github-secret";
const JIRA_SECRET: &str = "jira-secret";
const GITHUB_ISSUES_FIXTURE: &[u8] =
    include_bytes!("../../df-trackers/tests/fixtures/github-issues.json");
const JIRA_FIXTURE: &[u8] = include_bytes!("../../df-trackers/tests/fixtures/jira-automation.json");
const UNKNOWN_GITHUB_INSTALLATION_FIXTURE: &[u8] = br#"{
  "action":"opened",
  "installation":{"id":999999},
  "repository":{"full_name":"acme/api"},
  "issue":{"id":7001,"number":17,"title":"Implement webhook ingest","body":"Wire webhook verification into the tracker sync flow.","state":"open","labels":[{"name":"bug"},{"name":"trackers"}]}
}"#;

fn harness(pool: PgPool) -> Harness {
    let db = df_core::Db::from_pool(pool);
    let mut config = Config::new(PUBLIC_URL, RESOURCE);
    config.github_app_webhook_secret = Some(GITHUB_SECRET.into());
    let webauthn = df_web::relying_party(&config).expect("relying party");
    let state = AppState::new(db.clone(), cipher(), webauthn, config);

    Harness {
        db,
        router: df_web::router(state),
        cipher: cipher(),
    }
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn github_webhooks_are_public_and_acknowledged_when_verified(pool: PgPool) {
    let h = harness(pool);
    let org = h.db.create_org("acme", "Acme").await.unwrap();
    let user =
        h.db.upsert_user("owner@acme.test", Some("Owner"))
            .await
            .unwrap();
    h.db.add_member(org.id, user.id, Role::Owner).await.unwrap();

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Acme API".into()),
            remotes: vec!["git@github.com:acme/api.git".into()],
            created_by: Some(user.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let reply = post_webhook(
        &h,
        "/webhooks/github",
        GITHUB_ISSUES_FIXTURE,
        &[
            ("x-github-event", "issues"),
            (
                "x-hub-signature-256",
                &format!("sha256={}", github_signature(GITHUB_ISSUES_FIXTURE)),
            ),
        ],
    )
    .await;

    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(body_json(reply).await["ok"], serde_json::Value::Bool(true));
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn github_rejection_and_unknown_connection_share_the_same_public_404(pool: PgPool) {
    let h = harness(pool);
    let org = h.db.create_org("acme", "Acme").await.unwrap();
    let user =
        h.db.upsert_user("owner@acme.test", Some("Owner"))
            .await
            .unwrap();
    h.db.add_member(org.id, user.id, Role::Owner).await.unwrap();

    let mut tx = h.db.begin(org.id).await.unwrap();
    upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let invalid_signature = post_webhook(
        &h,
        "/webhooks/github",
        GITHUB_ISSUES_FIXTURE,
        &[
            ("x-github-event", "issues"),
            ("x-hub-signature-256", "sha256=deadbeef"),
        ],
    )
    .await;
    assert_eq!(invalid_signature.status(), StatusCode::NOT_FOUND);

    let unknown_installation = post_webhook(
        &h,
        "/webhooks/github",
        UNKNOWN_GITHUB_INSTALLATION_FIXTURE,
        &[
            ("x-github-event", "issues"),
            (
                "x-hub-signature-256",
                &format!(
                    "sha256={}",
                    github_signature(UNKNOWN_GITHUB_INSTALLATION_FIXTURE)
                ),
            ),
        ],
    )
    .await;

    assert_eq!(unknown_installation.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_text(invalid_signature).await,
        body_text(unknown_installation).await
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_labelled_issue_webhook_creates_one_job_and_a_stale_redelivery_does_not_duplicate_it(
    pool: PgPool,
) {
    let h = harness(pool);
    let org = h.db.create_org("acme", "Acme").await.unwrap();
    let user =
        h.db.upsert_user("owner@acme.test", Some("Owner"))
            .await
            .unwrap();
    h.db.add_member(org.id, user.id, Role::Owner).await.unwrap();

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Acme API".into()),
            remotes: vec!["git@github.com:acme/api.git".into()],
            created_by: Some(user.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "123456", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    for _ in 0..2 {
        let reply = post_webhook(
            &h,
            "/webhooks/github",
            GITHUB_ISSUES_FIXTURE,
            &[
                ("x-github-event", "issues"),
                (
                    "x-hub-signature-256",
                    &format!("sha256={}", github_signature(GITHUB_ISSUES_FIXTURE)),
                ),
            ],
        )
        .await;
        assert_eq!(reply.status(), StatusCode::OK);
    }

    let mut tx = h.db.begin(org.id).await.unwrap();
    let job = tx
        .get_job_by_ticket_for_repo(repo.id, Tracker::Github, "acme/api#17")
        .await
        .unwrap()
        .expect("synced job");
    let jobs = tx
        .list_jobs(&df_core::jobs::JobFilter {
            repo_id: Some(repo.id),
            ..Default::default()
        })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(job.title, "Implement webhook ingest");
    assert_eq!(job.remote_revision.as_deref(), Some("2026-09-03T18:00:00Z"));
    assert_eq!(jobs.len(), 1);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn jira_webhooks_require_the_site_and_secret_but_acknowledge_a_valid_request(pool: PgPool) {
    let h = harness(pool);
    let org = h.db.create_org("globex", "Globex").await.unwrap();
    let user =
        h.db.upsert_user("owner@globex.test", Some("Owner"))
            .await
            .unwrap();
    h.db.add_member(org.id, user.id, Role::Owner).await.unwrap();
    let sealed_secret = h.cipher.seal(JIRA_SECRET.as_bytes()).unwrap();

    let mut tx = h.db.begin(org.id).await.unwrap();
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some("Globex API".into()),
            remotes: vec!["git@github.com:globex/api.git".into()],
            created_by: Some(user.id),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = upsert_connection(
        &mut tx,
        Provider::Jira,
        "cloud-123",
        None,
        Some(&sealed_secret),
    )
    .await
    .unwrap();
    upsert_binding(
        &mut tx,
        repo.id,
        Some(connection.id),
        Provider::Jira,
        "DF",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let ok = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-123",
        JIRA_FIXTURE,
        &[("x-df-webhook-secret", JIRA_SECRET)],
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);

    let wrong_secret = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-123",
        JIRA_FIXTURE,
        &[("x-df-webhook-secret", "wrong-secret")],
    )
    .await;
    let unknown_site = post_webhook(
        &h,
        "/webhooks/jira?site=cloud-999",
        JIRA_FIXTURE,
        &[("x-df-webhook-secret", JIRA_SECRET)],
    )
    .await;
    let missing_site = post_webhook(
        &h,
        "/webhooks/jira",
        JIRA_FIXTURE,
        &[("x-df-webhook-secret", JIRA_SECRET)],
    )
    .await;

    assert_eq!(wrong_secret.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown_site.status(), StatusCode::NOT_FOUND);
    assert_eq!(missing_site.status(), StatusCode::NOT_FOUND);

    let wrong_body = body_text(wrong_secret).await;
    assert_eq!(wrong_body, body_text(unknown_site).await);
    assert_eq!(wrong_body, body_text(missing_site).await);
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_openapi_document_mentions_the_webhook_route(pool: PgPool) {
    let h = harness(pool);
    let doc = common::Call::get("/api/openapi.json").send(&h.router).await;
    doc.expect(StatusCode::OK);
    assert!(doc.body["paths"]["/webhooks/{provider}"]["post"].is_object());
}

async fn post_webhook(
    h: &Harness,
    uri: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> http::Response<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    h.router
        .clone()
        .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(response: http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn github_signature(body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(GITHUB_SECRET.as_bytes()).unwrap();
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
