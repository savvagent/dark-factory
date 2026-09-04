//! The MCP surface, driven the way an agent drives it.
//!
//! Handlers are called directly with an injected [`Principal`], which is the
//! only part of a real request that matters to them — everything else about the
//! HTTP layer is `rmcp`'s and is not this crate's to re-test. The exceptions are
//! the last two tests, which drive the assembled router, because the `401` and
//! its `WWW-Authenticate` header *are* the onboarding path and a test that
//! skipped them would let the whole zero-install premise break silently.

use df_auth::tokens::{Principal, TokenKind};
use df_billing::Meter;
use df_core::ids::{OrgId, RepoId, UserId};
use df_core::jobs::Tracker;
use df_core::orgs::Role;
use df_core::trackers::{upsert_binding, upsert_connection, Provider};
use df_core::watch::Watcher;
use df_core::Db;
use df_mcp::server::Factory;
use df_mcp::tools;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use sqlx::PgPool;

const RESOURCE: &str = "https://mcp.dark-factory.test/mcp";
const PUBLIC: &str = "https://mcp.dark-factory.test";
const REMOTE: &str = "git@github.com:acme/api.git";
const UPGRADE_URL: &str = "https://mcp.dark-factory.test/settings/billing";

/// Everything a token can carry, for the tests that are not about scopes.
fn all_scopes() -> Vec<String> {
    df_auth::oauth::KNOWN_SCOPES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn principal(user: UserId, org: OrgId, scopes: Vec<String>) -> Principal {
    Principal {
        token_id: uuid::Uuid::new_v4(),
        user_id: user,
        org_id: org,
        client_id: Some("df_client_test".into()),
        scopes,
        kind: TokenKind::Oauth,
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
    }
}

/// The one piece of a real request a handler reads.
fn parts(p: &Principal) -> http::request::Parts {
    let (mut parts, ()) = http::Request::builder()
        .uri("/mcp")
        .body(())
        .unwrap()
        .into_parts();
    parts.extensions.insert(p.clone());
    parts
}

/// Parts carrying no principal — what a handler would see if the middleware
/// were missing.
fn anonymous_parts() -> http::request::Parts {
    http::Request::builder()
        .uri("/mcp")
        .body(())
        .unwrap()
        .into_parts()
        .0
}

/// Tool results are typed envelopes, one per tool, and none of them is `Debug`
/// — so `unwrap`/`unwrap_err` will not do. These also re-serialize the result
/// to JSON, which is what an agent actually receives and therefore what the
/// assertions below should be written against.
fn ok<T: serde::Serialize>(r: Result<Json<T>, ErrorData>) -> serde_json::Value {
    match r {
        Ok(v) => serde_json::to_value(v.0).expect("result did not serialize"),
        Err(e) => panic!("expected success, got {}: {}", e.code.0, e.message),
    }
}

fn err<T: serde::Serialize>(r: Result<Json<T>, ErrorData>) -> ErrorData {
    match r {
        Ok(v) => panic!(
            "expected a refusal, got {}",
            serde_json::to_value(v.0).unwrap_or_default()
        ),
        Err(e) => e,
    }
}

fn code_of(e: &ErrorData) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["code"].as_str())
        .unwrap_or_default()
        .to_string()
}

struct Env {
    factory: Factory,
    db: Db,
}

async fn env(pool: PgPool) -> (Env, Principal) {
    // Enforcement off, matching the milestone-1 default: recording is on,
    // refusing is not.
    env_metered(pool, Meter::new(false, UPGRADE_URL)).await
}

async fn env_metered(pool: PgPool, meter: Meter) -> (Env, Principal) {
    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.expect("watcher");

    let org = db.create_org("acme", "Acme").await.unwrap();
    let user = db.upsert_user("rob@acme.test", Some("Rob")).await.unwrap();
    db.add_member(org.id, user.id, Role::Owner).await.unwrap();

    let caller = principal(user.id, org.id, all_scopes());
    (
        Env {
            factory: Factory::new(db.clone(), watcher, meter),
            db,
        },
        caller,
    )
}

impl Env {
    /// Add a second member to the fixture org.
    async fn teammate(&self, org: OrgId, email: &str) -> Principal {
        let user = self.db.upsert_user(email, None).await.unwrap();
        self.db
            .add_member(org, user.id, Role::Member)
            .await
            .unwrap();
        principal(user.id, org, all_scopes())
    }

    /// Register the fixture repo through the tool surface.
    async fn register(&self, caller: &Principal) -> serde_json::Value {
        ok(self
            .factory
            .register_repo(
                Extension(parts(caller)),
                Parameters(tools::repos::RegisterRepoArgs {
                    slug: "api".into(),
                    name: Some("Acme API".into()),
                    remotes: vec![REMOTE.into()],
                    default_branch: None,
                    default_agent_type: None,
                }),
            )
            .await)["repo"]
            .clone()
    }

    /// This org's standing, read through the `usage` tool.
    async fn usage(&self, caller: &Principal) -> serde_json::Value {
        ok(self
            .factory
            .usage(Extension(parts(caller)), Parameters(tools::org::NoArgs {}))
            .await)["usage"]
            .clone()
    }

    async fn add_job(&self, caller: &Principal, title: &str) -> serde_json::Value {
        ok(self
            .factory
            .add_job(
                Extension(parts(caller)),
                Parameters(tools::jobs::AddJobArgs {
                    title: title.into(),
                    description: None,
                    repo: Some("api".into()),
                    remote: None,
                    ticket_ref: None,
                    agent_type: None,
                    metadata: None,
                    depends_on: vec![],
                }),
            )
            .await)["job"]
            .clone()
    }
}

// ----------------------------------------------------------------- identity

#[sqlx::test(migrations = "../df-core/migrations")]
async fn whoami_names_the_one_org_this_token_opens(pool: PgPool) {
    let (env, caller) = env(pool).await;

    let out = ok(env
        .factory
        .whoami(Extension(parts(&caller)), Parameters(tools::org::NoArgs {}))
        .await);

    assert_eq!(out["user"]["email"], "rob@acme.test");
    assert_eq!(out["org"]["slug"], "acme");
    assert_eq!(out["org"]["plan"], "free");
    assert_eq!(out["role"], "owner");
    assert_eq!(out["token"]["kind"], "oauth");
    assert!(out["token"]["scopes"]
        .as_array()
        .unwrap()
        .contains(&serde_json::Value::String("jobs:write".into())));
}

/// A handler reached without the middleware must say so in terms that stop an
/// agent debugging its own arguments.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_handler_without_a_principal_blames_the_server(pool: PgPool) {
    let (env, _caller) = env(pool).await;

    let e = err(env
        .factory
        .whoami(
            Extension(anonymous_parts()),
            Parameters(tools::org::NoArgs {}),
        )
        .await);

    assert_eq!(code_of(&e), "unauthenticated");
    assert!(e.message.contains("misconfiguration"));
}

// ------------------------------------------------------------------- scopes

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_read_only_token_can_look_but_not_touch(pool: PgPool) {
    let (env, owner) = env(pool).await;
    env.register(&owner).await;

    let reader = principal(
        owner.user_id,
        owner.org_id,
        vec!["jobs:read".into(), "repos:read".into()],
    );

    // Reads are fine.
    ok(env
        .factory
        .list_repos(
            Extension(parts(&reader)),
            Parameters(tools::repos::ListReposArgs {
                include_inactive: false,
            }),
        )
        .await);

    // Writes are not, and the error names the scope that is missing — the only
    // thing that lets an agent re-authorize correctly rather than give up.
    let e = err(env
        .factory
        .add_job(
            Extension(parts(&reader)),
            Parameters(tools::jobs::AddJobArgs {
                title: "should not happen".into(),
                repo: Some("api".into()),
                description: None,
                remote: None,
                ticket_ref: None,
                agent_type: None,
                metadata: None,
                depends_on: vec![],
            }),
        )
        .await);

    assert_eq!(code_of(&e), "insufficient_scope");
    assert!(e.message.contains("jobs:write"), "{}", e.message);
}

// --------------------------------------------------------------- repo anchor

/// The end-to-end shape of a working session: find the repo, queue work, see
/// what is claimable, take it, finish it.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_full_loop_from_remote_url_to_completed_job(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    // An agent knows its remote, not its slug — and not necessarily in the
    // spelling the repo was registered with.
    let repo = ok(env
        .factory
        .resolve_repo(
            Extension(parts(&caller)),
            Parameters(tools::repos::RepoRefArgs {
                repo: None,
                remote: Some("https://github.com/acme/api".into()),
            }),
        )
        .await);
    assert_eq!(repo["repo"]["slug"], "api");

    let job = env.add_job(&caller, "wire up the health endpoint").await;
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["status"], "pending");

    let ready = ok(env
        .factory
        .ready(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs {
                repo: Some("api".into()),
                remote: None,
            }),
        )
        .await);
    assert_eq!(ready["jobs"].as_array().unwrap().len(), 1);

    let claimed = ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("api-agent@ci-7".into()),
            }),
        )
        .await);
    assert_eq!(claimed["jobs"][0]["status"], "in-progress");
    assert_eq!(claimed["jobs"][0]["claimedByLabel"], "api-agent@ci-7");

    // Claimed work is no longer on offer.
    let ready = ok(env
        .factory
        .ready(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);
    assert!(ready["jobs"].as_array().unwrap().is_empty());

    let done = ok(env
        .factory
        .complete_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::CompleteJobArgs {
                job: id,
                result: Some("added /healthz".into()),
            }),
        )
        .await);
    assert_eq!(done["job"]["status"], "completed");
    assert_eq!(done["job"]["result"], "added /healthz");

    let stats = ok(env
        .factory
        .stats(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);
    assert_eq!(stats["stats"]["completed"], 1);
    assert_eq!(stats["stats"]["pending"], 0);
}

/// Most jobs have no linked ticket. Their hot path must stay the same cheap
/// queue transition it was before tracker write-back existed.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn ticketless_job_transitions_still_succeed(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let job = env.add_job(&caller, "plain queue work").await;
    let id = job["id"].as_str().unwrap().to_string();

    let claimed = ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);
    assert_eq!(claimed["jobs"][0]["status"], "in-progress");

    let failed = ok(env
        .factory
        .fail_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::FailJobArgs {
                job: id,
                error: Some("failed locally".into()),
            }),
        )
        .await);
    assert_eq!(failed["job"]["status"], "failed");
    assert_eq!(failed["job"]["error"], "failed locally");
}

/// Dependencies gate claiming, and the surface has to report the gate rather
/// than letting an agent take work whose prerequisites are unfinished.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_blocked_job_is_not_offered_and_cannot_be_claimed(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let first = env.add_job(&caller, "run the migration").await;
    let second = env.add_job(&caller, "deploy the service").await;
    let first_id = first["id"].as_str().unwrap().to_string();
    let second_id = second["id"].as_str().unwrap().to_string();

    let deps = ok(env
        .factory
        .set_dependencies(
            Extension(parts(&caller)),
            Parameters(tools::jobs::SetDependenciesArgs {
                job: second_id.clone(),
                add: vec![first_id.clone()],
                remove: vec![],
            }),
        )
        .await);
    assert_eq!(deps["job"], second_id);
    assert_eq!(deps["dependencies"][0], first_id);

    let ready = ok(env
        .factory
        .ready(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);
    assert_eq!(
        ready["jobs"].as_array().unwrap().len(),
        1,
        "only the first is ready"
    );
    assert_eq!(ready["jobs"][0]["id"], first_id);

    let blocked = ok(env
        .factory
        .blocked(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);
    assert_eq!(blocked["jobs"][0]["id"], second_id);

    assert!(
        env.factory
            .claim_jobs(
                Extension(parts(&caller)),
                Parameters(tools::jobs::ClaimJobsArgs {
                    jobs: vec![second_id.clone()],
                    agent: None,
                }),
            )
            .await
            .is_err(),
        "a blocked job must not be claimable"
    );

    // Finish the prerequisite and the gate opens.
    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![first_id.clone()],
                agent: None,
            }),
        )
        .await);
    ok(env
        .factory
        .complete_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::CompleteJobArgs {
                job: first_id,
                result: None,
            }),
        )
        .await);

    let ready = ok(env
        .factory
        .ready(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);
    assert_eq!(ready["jobs"][0]["id"], second_id);
}

/// The error an agent hits on its first call from an unregistered checkout. It
/// has to contain the way out, or the agent is stuck.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn an_unresolvable_repo_says_what_is_registered_and_what_to_call(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let e = err(env
        .factory
        .add_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::AddJobArgs {
                title: "work".into(),
                repo: None,
                remote: Some("git@github.com:someone/else.git".into()),
                description: None,
                ticket_ref: None,
                agent_type: None,
                metadata: None,
                depends_on: vec![],
            }),
        )
        .await);

    assert_eq!(code_of(&e), "repo_unresolved");
    assert!(e.message.contains("api"), "must list the known slugs");
    assert!(e.message.contains("register_repo"), "must say what next");
}

// ---------------------------------------------------------- tenant isolation

/// The claim the whole product rests on, exercised through the surface a
/// customer actually reaches rather than through `df-core` directly.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn one_orgs_token_cannot_see_or_touch_anothers_work(pool: PgPool) {
    let (env, acme) = env(pool).await;
    env.register(&acme).await;
    let job = env.add_job(&acme, "acme secret work").await;
    let id = job["id"].as_str().unwrap().to_string();

    // A second tenant, with a token as privileged as one can be.
    let globex = env.db.create_org("globex", "Globex").await.unwrap();
    let eve = env.db.upsert_user("eve@globex.test", None).await.unwrap();
    env.db
        .add_member(globex.id, eve.id, Role::Owner)
        .await
        .unwrap();
    let intruder = principal(eve.id, globex.id, all_scopes());

    let jobs = ok(env
        .factory
        .list_jobs(
            Extension(parts(&intruder)),
            Parameters(tools::jobs::ListJobsArgs {
                status: None,
                repo: None,
                remote: None,
                mine: false,
                limit: None,
            }),
        )
        .await);
    assert!(jobs["jobs"].as_array().unwrap().is_empty());

    let repos = ok(env
        .factory
        .list_repos(
            Extension(parts(&intruder)),
            Parameters(tools::repos::ListReposArgs {
                include_inactive: true,
            }),
        )
        .await);
    assert!(repos["repos"].as_array().unwrap().is_empty());

    // Cannot fetch it by id, even knowing the id.
    assert!(env
        .factory
        .get_job(
            Extension(parts(&intruder)),
            Parameters(tools::jobs::JobArgs { job: id.clone() }),
        )
        .await
        .is_err());

    // Cannot claim it.
    assert!(env
        .factory
        .claim_jobs(
            Extension(parts(&intruder)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: None,
            }),
        )
        .await
        .is_err());

    // Cannot delete it.
    assert!(env
        .factory
        .delete_job(
            Extension(parts(&intruder)),
            Parameters(tools::jobs::JobArgs { job: id.clone() }),
        )
        .await
        .is_err());

    // And the original is untouched.
    let after = ok(env
        .factory
        .get_job(
            Extension(parts(&acme)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);
    assert_eq!(after["job"]["status"], "pending");
}

/// `users` is global — one human, one row, however many orgs — so an email
/// lookup is the one place a tool could reach across the tenant boundary
/// without reading another org's rows at all. The refusal must also not
/// confirm that the address exists.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_message_cannot_be_addressed_to_someone_in_another_org(pool: PgPool) {
    let (env, caller) = env(pool).await;

    let globex = env.db.create_org("globex", "Globex").await.unwrap();
    let outsider = env.db.upsert_user("cfo@globex.test", None).await.unwrap();
    env.db
        .add_member(globex.id, outsider.id, Role::Member)
        .await
        .unwrap();

    let to = |address: &str| tools::coord::SendMessageArgs {
        body: "hello".into(),
        to: Some(address.into()),
        kind: None,
        repo: None,
        remote: None,
        job: None,
        in_reply_to: None,
        agent: None,
    };

    let existing = err(env
        .factory
        .send_message(Extension(parts(&caller)), Parameters(to("cfo@globex.test")))
        .await);
    let absent = err(env
        .factory
        .send_message(
            Extension(parts(&caller)),
            Parameters(to("nobody@nowhere.test")),
        )
        .await);

    assert_eq!(code_of(&existing), "not_a_member");
    assert!(existing.message.contains("no member of this organization"));
    assert_eq!(
        existing.message.replace("cfo@globex.test", "X"),
        absent.message.replace("nobody@nowhere.test", "X"),
        "a real address in another org must be indistinguishable from an unused one"
    );
}

// --------------------------------------------------------------- coordination

#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_held_lease_names_its_holder_to_the_next_agent(pool: PgPool) {
    let (env, first) = env(pool).await;
    env.register(&first).await;

    let lease = ok(env
        .factory
        .acquire_lease(
            Extension(parts(&first)),
            Parameters(tools::coord::AcquireLeaseArgs {
                branch: "main".into(),
                repo: Some("api".into()),
                remote: None,
                agent: Some("agent-one".into()),
                job: None,
                ttl_seconds: Some(300),
            }),
        )
        .await);

    let mate = env.teammate(first.org_id, "sam@acme.test").await;
    let take = || tools::coord::AcquireLeaseArgs {
        branch: "main".into(),
        repo: Some("api".into()),
        remote: None,
        agent: Some("agent-two".into()),
        job: None,
        ttl_seconds: None,
    };

    let e = err(env
        .factory
        .acquire_lease(Extension(parts(&mate)), Parameters(take()))
        .await);

    assert_eq!(code_of(&e), "lease_held");
    assert!(
        e.message.contains("agent-one"),
        "the holder must be named so the caller can decide what to do: {}",
        e.message
    );
    assert_eq!(
        e.data.as_ref().unwrap()["retriable"],
        true,
        "a lease expires, so retrying is a sensible thing to do"
    );

    // A different branch of the same repo is free, so one agent does not block
    // the repository.
    ok(env
        .factory
        .acquire_lease(
            Extension(parts(&mate)),
            Parameters(tools::coord::AcquireLeaseArgs {
                branch: "feature/x".into(),
                ..take()
            }),
        )
        .await);

    // Released, the contested branch is free too.
    ok(env
        .factory
        .release_lease(
            Extension(parts(&first)),
            Parameters(tools::coord::ReleaseLeaseArgs {
                lease: lease["lease"]["id"].as_str().unwrap().into(),
            }),
        )
        .await);
    ok(env
        .factory
        .acquire_lease(Extension(parts(&mate)), Parameters(take()))
        .await);

    let live = ok(env
        .factory
        .list_leases(
            Extension(parts(&first)),
            Parameters(tools::coord::RepoScopeArgs::default()),
        )
        .await);
    assert_eq!(
        live["leases"].as_array().unwrap().len(),
        2,
        "both of sam's leases"
    );
}

#[sqlx::test(migrations = "../df-core/migrations")]
async fn messages_reach_the_inbox_and_the_cursor_clears_them(pool: PgPool) {
    let (env, sender) = env(pool).await;
    let mate = env.teammate(sender.org_id, "sam@acme.test").await;

    ok(env
        .factory
        .send_message(
            Extension(parts(&sender)),
            Parameters(tools::coord::SendMessageArgs {
                body: "migration is half applied on feature/x".into(),
                to: Some("sam@acme.test".into()),
                kind: Some("request".into()),
                repo: None,
                remote: None,
                job: None,
                in_reply_to: None,
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    let unread = ok(env
        .factory
        .unread_count(Extension(parts(&mate)), Parameters(tools::coord::NoArgs {}))
        .await);
    assert_eq!(unread["unread"], 1);

    // The sender does not see their own message as unread.
    let mine = ok(env
        .factory
        .unread_count(
            Extension(parts(&sender)),
            Parameters(tools::coord::NoArgs {}),
        )
        .await);
    assert_eq!(mine["unread"], 0);

    let inbox = ok(env
        .factory
        .inbox(
            Extension(parts(&mate)),
            Parameters(tools::coord::InboxArgs {
                unread_only: true,
                limit: None,
                newest_first: false,
            }),
        )
        .await);
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["kind"], "request");
    assert_eq!(messages[0]["senderLabel"], "agent-one");

    let cursor = ok(env
        .factory
        .ack_messages(
            Extension(parts(&mate)),
            Parameters(tools::coord::AckMessagesArgs {
                up_to: messages[0]["id"].as_i64().unwrap(),
            }),
        )
        .await);
    assert_eq!(cursor["cursor"], messages[0]["id"]);

    let unread = ok(env
        .factory
        .unread_count(Extension(parts(&mate)), Parameters(tools::coord::NoArgs {}))
        .await);
    assert_eq!(unread["unread"], 0);
}

/// `watch` has to return rather than hang when nothing happens, or an agent's
/// first use of it looks like the server died.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn watch_returns_timeout_when_the_queue_is_quiet(pool: PgPool) {
    let (env, caller) = env(pool).await;

    let out = ok(env
        .factory
        .watch(
            Extension(parts(&caller)),
            Parameters(tools::coord::WatchArgs {
                timeout_seconds: Some(1),
            }),
        )
        .await);

    assert_eq!(out["outcome"], "timeout");
    assert_eq!(out["waitedSeconds"], 1);
}

/// The point of the tool: an agent sitting in `watch` learns about work queued
/// by someone else without polling for it.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn watch_wakes_when_another_agent_queues_work(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let waiting = {
        let factory = env.factory.clone();
        let caller = caller.clone();
        tokio::spawn(async move {
            factory
                .watch(
                    Extension(parts(&caller)),
                    Parameters(tools::coord::WatchArgs {
                        timeout_seconds: Some(10),
                    }),
                )
                .await
                .map(|j| serde_json::to_value(j.0).unwrap())
                .map_err(|e| e.message.to_string())
        })
    };

    // Let the waiter subscribe before the change happens. A change published
    // with nobody listening is dropped — correct behaviour, and a race here
    // would make this test flaky rather than reveal a bug.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    env.add_job(&caller, "something new").await;

    let out = tokio::time::timeout(std::time::Duration::from_secs(12), waiting)
        .await
        .expect("watch never returned")
        .expect("watch task panicked")
        .expect("watch failed");

    assert_eq!(out["outcome"], "changed");
    // The field says how long the call blocked, not what it was allowed to. An
    // agent pacing itself off a wake-up must not be told it waited ten seconds.
    assert!(
        out["waitedSeconds"].as_u64().expect("waitedSeconds") < 10,
        "waitedSeconds reported the timeout budget instead of the real wait: {out}"
    );
}

// --------------------------------------------------------------- the surface

/// The tool list is the API. A tool silently disappearing — a router not added,
/// a macro attribute mistyped — breaks every caller and compiles fine.
#[test]
fn the_advertised_surface_is_exactly_what_the_design_specifies() {
    let router = tools::router();
    let mut names: Vec<String> = router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();

    let mut expected = vec![
        // Repos
        "register_repo",
        "list_repos",
        "resolve_repo",
        "update_repo",
        // Jobs
        "add_job",
        "get_job",
        "list_jobs",
        "update_job",
        "delete_job",
        "claim_jobs",
        "complete_job",
        "fail_job",
        "repend_job",
        "set_dependencies",
        "ready",
        "blocked",
        "stats",
        "link_ticket",
        "sync_ticket",
        // Coordination
        "acquire_lease",
        "renew_lease",
        "release_lease",
        "list_leases",
        "send_message",
        "inbox",
        "ack_messages",
        "unread_count",
        "watch",
        // Org
        "whoami",
        "usage",
    ];
    expected.sort();

    assert_eq!(names, expected);
}

/// Descriptions are the only documentation an agent gets, and the input schema
/// is what it builds arguments from. A tool missing either gets called wrongly.
#[test]
fn every_tool_documents_itself() {
    for tool in tools::router().list_all() {
        let description = tool
            .description
            .as_deref()
            .unwrap_or_else(|| panic!("{} has no description", tool.name));
        assert!(
            description.len() > 40,
            "{}'s description is too thin to guide a caller: {description:?}",
            tool.name
        );
        assert!(
            tool.input_schema.contains_key("type") || tool.input_schema.contains_key("properties"),
            "{} has no usable input schema",
            tool.name
        );
    }
}

// ------------------------------------------------------------ the front door

/// The `401` that makes zero-install onboarding work. An agent configured with
/// nothing but the MCP URL finds the authorization server through this header
/// and nowhere else.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn unauthenticated_requests_are_told_where_to_authenticate(pool: PgPool) {
    use tower::ServiceExt;

    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();
    let app = df_mcp::router(db, watcher, df_mcp::Config::new(RESOURCE, PUBLIC));

    let response = app
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "mcp.dark-factory.test")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .expect("no WWW-Authenticate header — clients cannot discover the AS")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(
            r#"resource_metadata="https://mcp.dark-factory.test/.well-known/oauth-protected-resource""#
        ),
        "{challenge}"
    );

    // And the document it points at is reachable without a token, or the
    // pointer is a closed loop.
    let metadata = app
        .oneshot(
            http::Request::builder()
                .uri("/.well-known/oauth-protected-resource")
                .header("host", "mcp.dark-factory.test")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(metadata.status(), http::StatusCode::OK);
    let body = axum::body::to_bytes(metadata.into_body(), 64 * 1024)
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["resource"], RESOURCE);
    assert_eq!(doc["authorization_servers"][0], PUBLIC);
}

/// A token minted for somebody else's resource must not open this one. The
/// confused-deputy defense, through the real middleware.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_token_for_another_resource_is_refused(pool: PgPool) {
    use tower::ServiceExt;

    let db = Db::from_pool(pool.clone());
    let watcher = Watcher::spawn(pool).await.unwrap();

    let org = db.create_org("acme", "Acme").await.unwrap();
    let user = db.upsert_user("rob@acme.test", None).await.unwrap();
    db.add_member(org.id, user.id, Role::Owner).await.unwrap();

    let (foreign_token, _) = df_auth::tokens::mint_pat(
        &db,
        user.id,
        org.id,
        "elsewhere",
        &["jobs:read".to_string()],
        "https://someone-else.test/mcp",
        None,
    )
    .await
    .unwrap();

    let app = df_mcp::router(db, watcher, df_mcp::Config::new(RESOURCE, PUBLIC));
    let response = app
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "mcp.dark-factory.test")
                .header("authorization", format!("Bearer {foreign_token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(http::header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains("different resource"),
        "the caller needs to know that re-authenticating will not help: {challenge}"
    );
}

// ------------------------------------------------------------------ metering

/// The tool surface and the price list are two lists in two crates that have to
/// agree. Nothing else notices when they stop.
#[test]
fn every_tool_has_a_price() {
    let names: Vec<String> = tools::router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    let problems = df_billing::classify::exhaustive_over(names.iter().map(String::as_str));
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The rule as customers are told it: you pay for work performed, not for
/// looking. Both kinds are recorded either way, so the split can be repriced
/// later against real history.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn work_is_billed_and_looking_is_not(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let before = env.usage(&caller).await;
    let billable_before = before["billableUsed"].as_i64().unwrap();
    let total_before = before["totalCalls"].as_i64().unwrap();

    // One billable call.
    env.add_job(&caller, "work").await;

    // Three free ones.
    for _ in 0..3 {
        ok(env
            .factory
            .ready(
                Extension(parts(&caller)),
                Parameters(tools::jobs::RepoScopeArgs::default()),
            )
            .await);
    }

    let after = env.usage(&caller).await;
    assert_eq!(
        after["billableUsed"].as_i64().unwrap() - billable_before,
        1,
        "only the add_job should have consumed the bucket"
    );
    assert_eq!(
        after["totalCalls"].as_i64().unwrap() - total_before,
        // add_job + 3 reads + the `usage` call that produced `after`.
        5,
        "every call is recorded, billable or not"
    );
    assert!(after["remaining"].as_i64().unwrap() < after["includedOps"].as_i64().unwrap());
}

/// The half of the billing promise that only a shared transaction can deliver:
/// the meter and the work commit or roll back together.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn a_failed_call_is_not_billed(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    let before = env.usage(&caller).await;

    // Billable, well-formed, and doomed: the remote resolves to nothing.
    let e = err(env
        .factory
        .add_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::AddJobArgs {
                title: "work".into(),
                repo: None,
                remote: Some("git@github.com:someone/else.git".into()),
                description: None,
                ticket_ref: None,
                agent_type: None,
                metadata: None,
                depends_on: vec![],
            }),
        )
        .await);
    assert_eq!(code_of(&e), "repo_unresolved");

    let after = env.usage(&caller).await;
    assert_eq!(
        after["billableUsed"], before["billableUsed"],
        "a call that failed must not have consumed the bucket"
    );
    assert_eq!(
        after["totalCalls"].as_i64().unwrap() - before["totalCalls"].as_i64().unwrap(),
        1,
        "the failed call left no trace at all — only the second usage call counted"
    );
}

/// `whoami` is the one tool an agent reliably calls at the start of a session,
/// so it is where finding out about the allowance is still useful.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn whoami_reports_the_allowance(pool: PgPool) {
    let (env, caller) = env(pool).await;

    let out = ok(env
        .factory
        .whoami(Extension(parts(&caller)), Parameters(tools::org::NoArgs {}))
        .await);

    assert_eq!(out["usage"]["plan"], "Free");
    assert_eq!(out["usage"]["includedOps"], 500);
    assert_eq!(out["usage"]["hardStop"], true);
    assert_eq!(
        out["usage"]["enforced"], false,
        "enforcement is off by default for milestone 1"
    );
    assert_eq!(out["usage"]["warning"], false);
}

/// Past the bucket on a hard-stop plan, work is refused and reading is not —
/// so an org that runs out mid-task can still see its own queue, understand
/// what happened, and go and upgrade.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn enforcement_stops_work_but_never_reads(pool: PgPool) {
    let (env, caller) = env_metered(pool, Meter::new(true, UPGRADE_URL)).await;
    env.register(&caller).await;

    // Spend the Free plan's entire bucket.
    sqlx::query(
        "INSERT INTO org_period_usage (org_id, period_start, billable_count, total_count) \
         VALUES ($1, date_trunc('month', now() AT TIME ZONE 'utc')::date, 500, 500) \
         ON CONFLICT (org_id, period_start) DO UPDATE SET billable_count = 500",
    )
    .bind(caller.org_id)
    .execute(env.db.pool())
    .await
    .unwrap();

    let e = err(env
        .factory
        .add_job(
            Extension(parts(&caller)),
            Parameters(tools::jobs::AddJobArgs {
                title: "one job too many".into(),
                repo: Some("api".into()),
                description: None,
                remote: None,
                ticket_ref: None,
                agent_type: None,
                metadata: None,
                depends_on: vec![],
            }),
        )
        .await);

    assert_eq!(code_of(&e), "quota_exceeded");
    assert_eq!(
        e.data.as_ref().unwrap()["retriable"],
        false,
        "retrying an exhausted bucket only burns the agent's time"
    );
    assert!(e.message.contains("500"), "{}", e.message);
    assert!(e.message.contains(UPGRADE_URL), "{}", e.message);

    // Reads keep working, which is the point.
    ok(env
        .factory
        .ready(
            Extension(parts(&caller)),
            Parameters(tools::jobs::RepoScopeArgs::default()),
        )
        .await);

    let usage = env.usage(&caller).await;
    assert_eq!(usage["remaining"], 0);
    assert_eq!(usage["warning"], true);
    assert_eq!(usage["enforced"], true);
}

/// The same org, one operation short of the limit, must be allowed the call
/// that lands exactly on it. A plan sold as 500 operations has to deliver 500.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn the_last_included_operation_is_allowed(pool: PgPool) {
    let (env, caller) = env_metered(pool, Meter::new(true, UPGRADE_URL)).await;
    env.register(&caller).await;

    sqlx::query(
        "INSERT INTO org_period_usage (org_id, period_start, billable_count, total_count) \
         VALUES ($1, date_trunc('month', now() AT TIME ZONE 'utc')::date, 499, 499) \
         ON CONFLICT (org_id, period_start) DO UPDATE SET billable_count = 499",
    )
    .bind(caller.org_id)
    .execute(env.db.pool())
    .await
    .unwrap();

    env.add_job(&caller, "the five hundredth").await;

    let usage = env.usage(&caller).await;
    assert_eq!(usage["billableUsed"], 500);
    assert_eq!(usage["remaining"], 0);
}

/// Enforcement off is the milestone-1 default, and it must mean *recorded but
/// not refused* rather than "not counted".
#[sqlx::test(migrations = "../df-core/migrations")]
async fn with_enforcement_off_an_over_budget_org_keeps_working(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;

    sqlx::query(
        "INSERT INTO org_period_usage (org_id, period_start, billable_count, total_count) \
         VALUES ($1, date_trunc('month', now() AT TIME ZONE 'utc')::date, 9_000, 9_000) \
         ON CONFLICT (org_id, period_start) DO UPDATE SET billable_count = 9000",
    )
    .bind(caller.org_id)
    .execute(env.db.pool())
    .await
    .unwrap();

    env.add_job(&caller, "well past the bucket").await;

    let usage = env.usage(&caller).await;
    assert_eq!(usage["billableUsed"], 9001);
    assert_eq!(usage["remaining"], 0);
    assert_eq!(usage["warning"], true);
    assert_eq!(usage["enforced"], false);
}

/// A tenant's meter is its own. Two orgs sharing a counter would be a billing
/// error and a data leak in the same bug.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn usage_is_counted_per_org(pool: PgPool) {
    let (env, acme) = env(pool).await;
    env.register(&acme).await;
    env.add_job(&acme, "acme work").await;
    env.add_job(&acme, "more acme work").await;

    let globex = env.db.create_org("globex", "Globex").await.unwrap();
    let eve = env.db.upsert_user("eve@globex.test", None).await.unwrap();
    env.db
        .add_member(globex.id, eve.id, Role::Owner)
        .await
        .unwrap();
    let other = principal(eve.id, globex.id, all_scopes());

    let theirs = env.usage(&other).await;
    assert_eq!(
        theirs["billableUsed"], 0,
        "another org's work must not appear on this org's meter"
    );

    let ours = env.usage(&acme).await;
    assert_eq!(
        ours["billableUsed"], 3,
        "register_repo plus two add_jobs; reading usage itself is free"
    );
}

// ------------------------------------------------------------- link_ticket

/// The ordinary case: a job queued by hand gets a tracker ticket attached
/// after the fact, and the loop-safety `remote_revision` a stale echo would
/// have relied on is cleared so the ticket's first real webhook is not
/// mistaken for one.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn link_ticket_attaches_a_tracker_and_ticket_ref(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let job = env.add_job(&caller, "wire up the webhook").await;
    let id = job["id"].as_str().unwrap().to_string();

    let linked = ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id,
                tracker: Tracker::Github,
                ticket_ref: "acme/api#42".into(),
            }),
        )
        .await);

    assert_eq!(linked["job"]["tracker"], "github");
    assert_eq!(linked["job"]["ticketRef"], "acme/api#42");
}

/// A blank `ticket_ref` is rejected before it ever reaches the database —
/// storing one would make every job that hasn't been linked yet
/// indistinguishable from one deliberately linked to nothing.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn link_ticket_rejects_a_blank_ticket_ref(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let job = env.add_job(&caller, "needs a ticket").await;
    let id = job["id"].as_str().unwrap().to_string();

    let e = err(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id,
                tracker: Tracker::Jira,
                ticket_ref: "   ".into(),
            }),
        )
        .await);

    assert!(e.message.contains("ticket_ref"), "{}", e.message);
}

/// Two jobs cannot both claim the same ticket — that is a genuine conflict
/// between two explicit calls, not a race to converge on, so it is named
/// rather than silently resolved.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn link_ticket_refuses_a_ticket_another_job_already_holds(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let first = env.add_job(&caller, "first job").await;
    let second = env.add_job(&caller, "second job").await;
    let first_id = first["id"].as_str().unwrap().to_string();
    let second_id = second["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: first_id,
                tracker: Tracker::Github,
                ticket_ref: "acme/api#7".into(),
            }),
        )
        .await);

    let e = err(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: second_id,
                tracker: Tracker::Github,
                ticket_ref: "acme/api#7".into(),
            }),
        )
        .await);

    assert_eq!(code_of(&e), "ticket_already_linked");
}

/// The tool requires the `trackers` scope, and the error names it — the same
/// discipline every other write tool follows.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn link_ticket_requires_the_trackers_scope(pool: PgPool) {
    let (env, owner) = env(pool).await;
    env.register(&owner).await;
    let job = env.add_job(&owner, "needs a scope check").await;
    let id = job["id"].as_str().unwrap().to_string();

    let reader = principal(
        owner.user_id,
        owner.org_id,
        vec!["jobs:read".into(), "jobs:write".into(), "repos:read".into()],
    );

    let e = err(env
        .factory
        .link_ticket(
            Extension(parts(&reader)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id,
                tracker: Tracker::Github,
                ticket_ref: "acme/api#1".into(),
            }),
        )
        .await);

    assert_eq!(code_of(&e), "insufficient_scope");
    assert!(e.message.contains("trackers"), "{}", e.message);
}

// ------------------------------------------------------------- sync_ticket

/// `sync_ticket` is a caller-facing action: a job that has never been linked
/// gets a clear "call link_ticket first" error rather than silently doing
/// nothing.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_requires_a_tracker_link(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let job = env.add_job(&caller, "not linked yet").await;
    let id = job["id"].as_str().unwrap().to_string();

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert!(e.message.contains("link_ticket"), "{}", e.message);
}

/// A job that is still `pending` has nothing to report yet — there is no
/// status change to reflect on the ticket.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_refuses_a_still_pending_job(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let job = env.add_job(&caller, "still pending").await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Github,
                ticket_ref: "acme/api#9".into(),
            }),
        )
        .await);

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert!(e.message.contains("pending"), "{}", e.message);
}

/// A linked job whose repo has no tracker binding at all gets a
/// configuration error naming what to do next, not a silent no-op — unlike
/// the automatic post-transition sync, this call exists to talk to the
/// tracker, so nothing happening is itself the failure to report.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_without_a_binding_reports_not_configured(pool: PgPool) {
    let (env, caller) = env(pool).await;
    env.register(&caller).await;
    let job = env.add_job(&caller, "linked but unbound").await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Github,
                ticket_ref: "acme/api#11".into(),
            }),
        )
        .await);

    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert_eq!(code_of(&e), "invalid_argument");
    assert_eq!(e.data.as_ref().unwrap()["retriable"], false);
    assert!(e.message.contains("no active"), "{}", e.message);
}

/// A linked job whose repo has an active binding, but whose outbound call
/// cannot go through (here: the server has no GitHub App configured at all),
/// is reported as a distinct, retriable failure — unlike the fixed,
/// non-retriable configuration errors above, this is exactly the case
/// `sync_ticket`'s own description promises a caller can retry after.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_reports_an_outbound_failure_as_retriable(pool: PgPool) {
    let (env, caller) = env(pool).await;
    let repo = env.register(&caller).await;
    let repo_id: RepoId = repo["id"].as_str().unwrap().parse().unwrap();

    let mut tx = env.db.begin(caller.org_id).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "999999", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo_id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let job = env
        .add_job(&caller, "linked and bound, but no App configured")
        .await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Github,
                ticket_ref: "acme/api#13".into(),
            }),
        )
        .await);

    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert_eq!(code_of(&e), "tracker_sync_failed");
    assert_eq!(e.data.as_ref().unwrap()["retriable"], true);
}

/// The whole point of this issue: an org on a hard-stop plan already over its
/// bucket must be refused *before* `sync_ticket` posts anything to the
/// tracker, not after. The setup mirrors
/// `sync_ticket_reports_an_outbound_failure_as_retriable` exactly (a binding
/// is present, but the server has no GitHub App configured, so an outbound
/// call — if one were attempted — would fail with `tracker_sync_failed`).
/// Seeing `quota_exceeded` instead proves the pre-check ran first; if it
/// didn't, this would return `tracker_sync_failed` just like that test does.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_refuses_before_the_outbound_call_when_over_budget(pool: PgPool) {
    let (env, caller) = env_metered(pool, Meter::new(true, UPGRADE_URL)).await;
    let repo = env.register(&caller).await;
    let repo_id: RepoId = repo["id"].as_str().unwrap().parse().unwrap();

    let mut tx = env.db.begin(caller.org_id).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "999999", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo_id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Link and claim the job while the bucket still has room — these are
    // themselves billable calls, and the point of this test is the refusal
    // on `sync_ticket` specifically, not on getting the job into position.
    let job = env
        .add_job(&caller, "linked and bound, but the org is over budget")
        .await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Github,
                ticket_ref: "acme/api#21".into(),
            }),
        )
        .await);

    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    // Now spend the rest of the Free plan's bucket, so the org is exactly at
    // its limit by the time `sync_ticket` is called. This test has already
    // made a few real billable calls (add_job, link_ticket, claim_jobs), so
    // both billable_count and total_count are updated together here — an
    // update that only touched billable_count would leave total_count below
    // billable_count, an impossible state for this table since every
    // billable call is also a total one.
    sqlx::query(
        "INSERT INTO org_period_usage (org_id, period_start, billable_count, total_count) \
         VALUES ($1, date_trunc('month', now() AT TIME ZONE 'utc')::date, 500, 500) \
         ON CONFLICT (org_id, period_start) DO UPDATE SET billable_count = 500, total_count = 500",
    )
    .bind(caller.org_id)
    .execute(env.db.pool())
    .await
    .unwrap();

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert_eq!(
        code_of(&e),
        "quota_exceeded",
        "a quota_exceeded refusal proves the pre-check ran before the outbound call; \
         tracker_sync_failed would mean the tracker was contacted anyway: {}",
        e.message
    );
    assert_eq!(e.data.as_ref().unwrap()["retriable"], false);
}

/// A ticket_ref that isn't a valid GitHub issue reference ("owner/repo#N")
/// will never succeed no matter how many times it's retried — it must not
/// share the outbound-call path's retriable bucket, or an agent could poll
/// `sync_ticket` forever against a call that can never work. `link_ticket`
/// only rejects a blank ticket_ref, not a malformed one, so this shape is
/// reachable through the normal tool surface.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_reports_a_malformed_github_ticket_ref_as_non_retriable(pool: PgPool) {
    let (env, caller) = env(pool).await;
    let repo = env.register(&caller).await;
    let repo_id: RepoId = repo["id"].as_str().unwrap().parse().unwrap();

    let mut tx = env.db.begin(caller.org_id).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Github, "999999", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo_id,
        Some(connection.id),
        Provider::Github,
        "acme/api",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let job = env
        .add_job(&caller, "linked to a malformed ticket_ref")
        .await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Github,
                ticket_ref: "not-a-valid-github-ref".into(),
            }),
        )
        .await);

    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert_eq!(code_of(&e), "invalid_argument");
    assert_eq!(e.data.as_ref().unwrap()["retriable"], false);
    assert!(e.message.contains("not a valid GitHub"), "{}", e.message);
}

/// Same shape as the GitHub case above, for JIRA's own `PROJECT-123`
/// grammar: `link_ticket` doesn't validate the ticket_ref's format, only
/// that it's non-blank, so a caller can link a job to a key that will never
/// resolve against JIRA's API. The pre-check runs ahead of
/// `sync_jira_job`, so this never needs a working JIRA App configuration to
/// exercise.
#[sqlx::test(migrations = "../df-core/migrations")]
async fn sync_ticket_reports_a_malformed_jira_ticket_ref_as_non_retriable(pool: PgPool) {
    let (env, caller) = env(pool).await;
    let repo = env.register(&caller).await;
    let repo_id: RepoId = repo["id"].as_str().unwrap().parse().unwrap();

    let mut tx = env.db.begin(caller.org_id).await.unwrap();
    let connection = upsert_connection(&mut tx, Provider::Jira, "acme.atlassian.net", None, None)
        .await
        .unwrap();
    upsert_binding(
        &mut tx,
        repo_id,
        Some(connection.id),
        Provider::Jira,
        "ACME",
        "trackers",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let job = env
        .add_job(&caller, "linked to a malformed jira ticket_ref")
        .await;
    let id = job["id"].as_str().unwrap().to_string();

    ok(env
        .factory
        .link_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::LinkTicketArgs {
                job: id.clone(),
                tracker: Tracker::Jira,
                ticket_ref: "not-a-valid-jira-key".into(),
            }),
        )
        .await);

    ok(env
        .factory
        .claim_jobs(
            Extension(parts(&caller)),
            Parameters(tools::jobs::ClaimJobsArgs {
                jobs: vec![id.clone()],
                agent: Some("agent-one".into()),
            }),
        )
        .await);

    let e = err(env
        .factory
        .sync_ticket(
            Extension(parts(&caller)),
            Parameters(tools::jobs::JobArgs { job: id }),
        )
        .await);

    assert_eq!(code_of(&e), "invalid_argument");
    assert_eq!(e.data.as_ref().unwrap()["retriable"], false);
    assert!(e.message.contains("not a valid JIRA"), "{}", e.message);
}
