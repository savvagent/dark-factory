//! Shared fixtures. Every integration test needs at least one org with a user
//! and a repo, and the isolation tests need two orgs that must never see each
//! other.

#![allow(dead_code)]

use df_core::ids::{OrgId, RepoId, UserId};
use df_core::jobs::NewJob;
use df_core::orgs::Role;
use df_core::repos::NewRepo;
use df_core::Db;
use sqlx::PgPool;

pub struct Tenant {
    pub org: OrgId,
    pub user: UserId,
    pub repo: RepoId,
}

/// Create an org with one owner and one registered repo.
pub async fn tenant(db: &Db, slug: &str, remote: &str) -> Tenant {
    let org = db.create_org(slug, slug).await.expect("create org");
    let user = db
        .upsert_user(&format!("owner@{slug}.test"), Some("Owner"))
        .await
        .expect("create user");
    db.add_member(org.id, user.id, Role::Owner)
        .await
        .expect("add member");

    let mut tx = db.begin(org.id).await.expect("begin");
    let repo = tx
        .register_repo(NewRepo {
            slug: "api".into(),
            name: Some(format!("{slug} api")),
            remotes: vec![remote.into()],
            created_by: Some(user.id),
            ..Default::default()
        })
        .await
        .expect("register repo");
    tx.commit().await.expect("commit");

    Tenant {
        org: org.id,
        user: user.id,
        repo: repo.id,
    }
}

pub fn db(pool: PgPool) -> Db {
    Db::from_pool(pool)
}

/// A minimal job in this tenant's repo.
pub fn job(t: &Tenant, title: &str) -> NewJob {
    NewJob {
        repo_id: t.repo,
        title: title.into(),
        created_by: Some(t.user),
        ..Default::default()
    }
}
