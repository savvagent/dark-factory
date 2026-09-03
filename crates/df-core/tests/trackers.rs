//! Tracker connection and binding CRUD.

mod common;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::{db, tenant};
use df_core::crypto::Cipher;
use df_core::trackers::{
    delete_binding, delete_connection, get_binding, get_connection, resolve_binding,
    upsert_binding, upsert_connection, Provider,
};
use sqlx::PgPool;

#[sqlx::test]
async fn tracker_connections_and_bindings_round_trip(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let cipher = Cipher::from_base64_key(&B64.encode([7u8; 32])).unwrap();
    let credentials = cipher.seal(b"jira-refresh-token").unwrap();
    let webhook = cipher.seal(b"shared-secret").unwrap();

    let mut tx = db.begin(t.org).await.unwrap();

    let github = upsert_connection(
        &mut tx,
        t.org,
        Provider::Github,
        "installation-1",
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(github.external_id, "installation-1");
    assert!(github.encrypted_credentials.is_none());
    assert!(github.encrypted_webhook_secret.is_none());

    let jira = upsert_connection(
        &mut tx,
        t.org,
        Provider::Jira,
        "site-1",
        Some(&credentials),
        Some(&webhook),
    )
    .await
    .unwrap();
    assert_eq!(
        jira.encrypted_credentials.as_deref(),
        Some(
            B64.encode([credentials.nonce.clone(), credentials.ciphertext.clone()].concat())
                .as_str()
        )
    );
    assert_eq!(
        jira.encrypted_webhook_secret.as_deref(),
        Some(
            B64.encode([webhook.nonce.clone(), webhook.ciphertext.clone()].concat())
                .as_str()
        )
    );

    let fetched = get_connection(&mut tx, t.org, Provider::Jira)
        .await
        .unwrap()
        .expect("jira connection");
    assert_eq!(fetched, jira);

    let rebound = upsert_connection(
        &mut tx,
        t.org,
        Provider::Jira,
        "site-2",
        Some(&cipher.seal(b"rotated-refresh-token").unwrap()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(rebound.id, jira.id);
    assert_eq!(rebound.external_id, "site-2");
    assert!(rebound.encrypted_credentials.is_some());
    assert!(rebound.encrypted_webhook_secret.is_none());

    let binding = upsert_binding(
        &mut tx,
        t.org,
        t.repo,
        Some(github.id),
        Provider::Github,
        "acme/api",
    )
    .await
    .unwrap();
    assert_eq!(binding.connection_id, Some(github.id));

    let fetched = get_binding(&mut tx, t.org, binding.id)
        .await
        .unwrap()
        .expect("github binding");
    assert_eq!(fetched, binding);

    let resolved = resolve_binding(&mut tx, t.org, t.repo, Provider::Github)
        .await
        .unwrap()
        .expect("resolved github binding");
    assert_eq!(resolved, binding);

    delete_connection(&mut tx, t.org, Provider::Github)
        .await
        .unwrap();
    assert!(get_connection(&mut tx, t.org, Provider::Github)
        .await
        .unwrap()
        .is_none());

    let orphaned = get_binding(&mut tx, t.org, binding.id)
        .await
        .unwrap()
        .expect("binding survives");
    assert_eq!(orphaned.connection_id, None);

    delete_binding(&mut tx, t.org, binding.id).await.unwrap();
    assert!(get_binding(&mut tx, t.org, binding.id)
        .await
        .unwrap()
        .is_none());
    assert!(resolve_binding(&mut tx, t.org, t.repo, Provider::Github)
        .await
        .unwrap()
        .is_none());

    tx.commit().await.unwrap();
}
