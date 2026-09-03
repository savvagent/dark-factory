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

    let github = upsert_connection(&mut tx, Provider::Github, "installation-1", None, None)
        .await
        .unwrap();
    assert_eq!(github.external_id, "installation-1");
    assert!(github.encrypted_credentials.is_none());
    assert!(github.encrypted_webhook_secret.is_none());

    let jira = upsert_connection(
        &mut tx,
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

    let fetched = get_connection(&mut tx, Provider::Jira)
        .await
        .unwrap()
        .expect("jira connection");
    assert_eq!(fetched, jira);

    let rebound = upsert_connection(
        &mut tx,
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
        t.repo,
        Some(github.id),
        Provider::Github,
        "acme/api",
    )
    .await
    .unwrap();
    assert_eq!(binding.connection_id, Some(github.id));

    let fetched = get_binding(&mut tx, binding.id)
        .await
        .unwrap()
        .expect("github binding");
    assert_eq!(fetched, binding);

    let resolved = resolve_binding(&mut tx, t.repo, Provider::Github)
        .await
        .unwrap()
        .expect("resolved github binding");
    assert_eq!(resolved, binding);

    delete_connection(&mut tx, Provider::Github).await.unwrap();
    assert!(get_connection(&mut tx, Provider::Github)
        .await
        .unwrap()
        .is_none());

    let orphaned = get_binding(&mut tx, binding.id)
        .await
        .unwrap()
        .expect("binding survives");
    assert_eq!(orphaned.connection_id, None);

    delete_binding(&mut tx, binding.id).await.unwrap();
    assert!(get_binding(&mut tx, binding.id).await.unwrap().is_none());
    assert!(resolve_binding(&mut tx, t.repo, Provider::Github)
        .await
        .unwrap()
        .is_none());

    tx.commit().await.unwrap();
}

/// Guard 1 (the API shape), not guard 2 (RLS): a binding must not silently
/// point at a connection of the wrong provider, even within the caller's own
/// org, because the sync engine trusts `binding.provider` to pick the client
/// it dials. This is exercised at the `require_connection_in_org` level
/// directly, independent of row-level security, per the same-org same-caller
/// path an RLS test cannot cover.
#[sqlx::test]
async fn binding_rejects_a_connection_of_the_wrong_provider(pool: PgPool) {
    let db = db(pool);
    let t = tenant(&db, "acme", "git@github.com:acme/api.git").await;

    let mut tx = db.begin(t.org).await.unwrap();
    let jira = upsert_connection(&mut tx, Provider::Jira, "site-1", None, None)
        .await
        .unwrap();

    let err = upsert_binding(&mut tx, t.repo, Some(jira.id), Provider::Github, "acme/api")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("not found in this org"),
        "unexpected error: {err}"
    );
}

/// A binding must not accept a connection id that belongs to another org, even
/// though RLS already makes such a row invisible to the query inside
/// `require_connection_in_org`. This asserts the function's own contract
/// (Invalid, not silently succeeding or panicking) rather than relying on the
/// RLS test suite to be the only thing standing between orgs.
#[sqlx::test]
async fn binding_rejects_a_connection_from_another_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(b.org).await.unwrap();
    let globex_connection =
        upsert_connection(&mut tx, Provider::Github, "installation-9", None, None)
            .await
            .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(a.org).await.unwrap();
    let err = upsert_binding(
        &mut tx,
        a.repo,
        Some(globex_connection.id),
        Provider::Github,
        "acme/api",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("not found in this org"),
        "unexpected error: {err}"
    );
}

/// A binding must not accept a repo id that belongs to another org. `get_repo`
/// is scoped by the pinned transaction's own org, so this is really a test of
/// `require_repo_in_org`'s contract rather than a fresh isolation guarantee,
/// but the contract deserves its own coverage independent of the RLS suite.
#[sqlx::test]
async fn binding_rejects_a_repo_from_another_org(pool: PgPool) {
    let db = db(pool);
    let a = tenant(&db, "acme", "git@github.com:acme/api.git").await;
    let b = tenant(&db, "globex", "git@github.com:globex/api.git").await;

    let mut tx = db.begin(a.org).await.unwrap();
    let err = upsert_binding(&mut tx, b.repo, None, Provider::Github, "acme/api")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("repo not found"),
        "unexpected error: {err}"
    );
}
