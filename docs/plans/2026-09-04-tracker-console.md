# Milestone 2 Task 6 — the tracker console

**Spec:** [`docs/specs/2026-09-04-tracker-console-design.md`](../specs/2026-09-04-tracker-console-design.md)
— read it first. **Parent plan:** [`docs/plans/2026-09-03-df-trackers.md`](2026-09-03-df-trackers.md),
whose Task 6 this expands: the four checkboxes there assumed the REST routes existed. They
do not, so this splits into five sub-tasks, server first.

## Status — 2026-09-04

6.1 ✅ shipped, 6.2 ✅ shipped, 6.3 ✅ shipped, 6.4 ✅ shipped, 6.5 ⬜.

## Global Constraints

Everything in the parent plan's Global Constraints still holds. The ones this task will
actually collide with:

- **Every SQL statement lives in `df-core`.** The two new list functions go there; the
  handlers call them.
- **Guard 1 in every handler**: `state.db.begin(ctx.org.id)`, and `OrgCtx::require_admin`
  before the body does anything. No new tenant table, so no new RLS policy — but new
  cross-org tests, in both `df-core` and `df-web`.
- **An org you are not in is `404`, never `403`.**
- **No credential is ever spent on a `GET`.** Both OAuth redemptions are `POST`s;
  `every_single_use_redemption_is_a_post` must keep passing.
- **The router and the OpenAPI document are built from one list** — `catalog.rs`, with a
  summary and description per route, and a component per request/response body.
- **No AI attribution** anywhere. `cargo fmt --all` before every Rust commit. No `unwrap()`
  outside tests.
- **A breaking change to a public interface must be named explicitly.** This task has one:
  removing `trackerBinding` from the console's two repo request bodies (6.5, spec §7). It
  is named in the spec, must be named in the PR body, and must be raised with the architect
  reviewer.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/df-core/src/trackers.rs` | `list_connections`, `list_bindings_for_repo` |
| **Modify.** `crates/df-core/tests/trackers.rs` | tests for both, incl. cross-org |
| **Modify.** `crates/df-server/src/config.rs` | `DF_GITHUB_APP_SLUG`, `DF_GITHUB_APP_CLIENT_ID`, `DF_GITHUB_APP_CLIENT_SECRET` |
| **Modify.** `crates/df-server/src/lib.rs` | thread the new + existing JIRA vars into `df_web::Config` |
| **Modify.** `.env.example` | document all three, with the *why* and the App setting they depend on |
| **Modify.** `crates/df-trackers/src/github.rs` | `exchange_user_code`, `user_installations`, `verify_installation_access` |
| **Create.** `crates/df-web/src/routes/trackers.rs` | seven handlers + the view types |
| **Modify.** `crates/df-web/src/routes/mod.rs` | `pub mod trackers;` |
| **Modify.** `crates/df-web/src/state.rs` | new `Config` fields the handlers read |
| **Modify.** `crates/df-web/src/catalog.rs` | seven endpoints |
| **Modify.** `crates/df-web/src/openapi.rs` | components for the new bodies |
| **Modify.** `crates/df-web/tests/console.rs` | route tests: admin-only, cross-org `404`, redaction |
| **Create.** `web/src/routes/o/[org]/trackers/+page.svelte` | connections page |
| **Create.** `web/src/routes/trackers/callback/+page.svelte` | provider return page |
| **Create.** `web/src/lib/trackerState.ts` | the OAuth `state` this browser minted, and the check on return |
| **Modify.** `web/src/routes/o/[org]/repos/+page.svelte` | per-repo binding editor |
| **Modify.** `web/src/routes/o/[org]/+layout.svelte` | nav entry |
| **Modify.** `web/src/lib/api.ts`, `web/src/lib/types.ts` | client methods and types |
| **Modify.** `crates/df-web/src/routes/repos.rs`, `openapi.rs`, `web/src/lib/types.ts` | 6.5: drop `trackerBinding` |

## Task Order & Rationale

6.1 (df-core + config) has no consumer and changes no behavior. 6.2 (the GitHub
verification client) is pure `df-trackers` with unit tests against a mock server, and is
what 6.3 cannot be written honestly without. 6.3 (the REST surface) needs both. 6.4 (the
UI) reads whatever 6.3 exposed and is the only sub-task with no Rust test gate. 6.5 (the
`trackerBinding` removal) lands last and alone, because it is the one breaking change and
should be revertible without taking the feature with it.

---

## Task 6.1 — `df-core` list functions and deployment config ✅

**Files:** `crates/df-core/src/trackers.rs`, `crates/df-core/tests/trackers.rs`,
`crates/df-server/src/config.rs`, `crates/df-server/src/lib.rs`, `crates/df-web/src/state.rs`,
`.env.example`.

**Interfaces:** produces `df_core::trackers::{list_connections, list_bindings_for_repo}` and
`df_web::Config::{github_app_slug, github_app_client_id, github_app_client_secret,
jira_client_id, jira_client_secret}` for 6.3 to consume.

- [x] Write failing tests in `crates/df-core/tests/trackers.rs` first: `list_connections`
      returns both providers' rows for the org and none of another org's;
      `list_bindings_for_repo` returns a repo's bindings and refuses/returns empty for a repo
      in another org. Mirror the file's existing `#[sqlx::test]` shape.
- [x] Add both functions to `crates/df-core/src/trackers.rs`, each taking `&mut Tx<'_>`, each
      binding `org_id = $1` explicitly, reusing `CONNECTION_COLS`/`BINDING_COLS` and
      `validate_connection` exactly as the existing getters do.
- [x] Add the three `DF_GITHUB_APP_*` vars to `crates/df-server/src/config.rs` as
      `Option<String>`, following `github_app_webhook_secret`'s shape (`optional(...)`), with
      doc comments saying why each is optional.
- [x] Add the five fields to `df_web::Config` (`crates/df-web/src/state.rs`) and thread them
      from `crates/df-server/src/lib.rs`, including the two existing `DF_JIRA_*` values that
      `df-web` does not currently receive.
- [x] Document all three new vars in `.env.example` with the *why*, and state explicitly that
      the GitHub App must have "Request user authorization (OAuth) during installation"
      enabled or the connect flow cannot verify anything (spec §2, Risks).
- [x] `cargo test -p df-core --test trackers`, `cargo test --workspace`,
      `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`. Commit:
      `df-core: list tracker connections and a repo's bindings`.

## Task 6.2 — GitHub user-to-server verification ✅

**Files:** `crates/df-trackers/src/github.rs`.

**Interfaces:** produces `GithubAppClient::verify_installation_access(client_id,
client_secret, code, installation_id)` (or a small `GithubUserAuth` alongside the App client
— whichever reads better once written; the App client's JWT machinery is not involved, so a
free function taking the OAuth credentials is the likelier shape).

- [x] Write failing unit tests first, against the crate's existing mock-server pattern (see
      `jira.rs`'s tests for the shape): a code that exchanges to a token whose
      `/user/installations` contains the id → `Ok`; one that does not contain it → the
      "does not administer" error; an exchange that returns GitHub's `{"error": ...}` body →
      the "re-run the connect flow" error.
- [x] Implement the exchange (`POST https://github.com/login/oauth/access_token`,
      `Accept: application/json`) and `GET /user/installations`, paginating only as far as
      GitHub's default page — an account with >30 installations is not a case to engineer for
      now, but the code must not silently miss one, so follow `Link: rel="next"` if present.
- [x] Error text is written for the admin reading it in a browser, not for a log: say what
      failed, and that the next step is to re-run Connect GitHub.
- [x] `cargo test -p df-trackers`, `cargo clippy --all-targets -- -D warnings`,
      `cargo fmt --all`. Commit: `df-trackers: verify a GitHub installation belongs to the
      admin binding it`.

## Task 6.3 — the console REST surface ✅

**Files:** `crates/df-web/src/routes/trackers.rs` (new), `crates/df-web/src/routes/mod.rs`,
`crates/df-web/src/catalog.rs`, `crates/df-web/src/openapi.rs`,
`crates/df-web/tests/console.rs`.

**Interfaces:** produces the six endpoints in spec §3 for 6.4 to call.

- [x] Write failing tests in `crates/df-web/tests/console.rs` first, following the file's
      existing helpers: a member (non-admin) gets `403` on each admin route; an admin of
      another org gets `404` (never `403`) on all seven; `PUT …/tracker-binding` with a
      malformed `owner/repo` or project key is refused; a `GET` of the connections list
      never contains the string `encrypted`; `DELETE` answers `204`.
- [x] Add the view types (`TrackerConnectionView`, `TrackerConnectionsView`,
      `TrackerBindingView`) and the seven handlers. Every handler: `ctx.require_admin()?`
      (except the bindings `GET`), one `state.db.begin(ctx.org.id)`, one `tx.commit()`.
- [x] Audit entries for the four mutating routes, following `repos.rs`'s `Entry::new(...)`
      usage. Add the action constants to `df_core::audit::action` if they do not exist.
- [x] `external_ref` validation per provider (spec §3), with errors naming the expected shape.
- [x] Register all seven in `catalog.rs` with `Auth::OrgAdmin`/`OrgMember`, summaries,
      descriptions, `.status(204)` on the deletes, and components in `openapi.rs`.
- [x] `cargo test -p df-web`, `cargo test --workspace`,
      `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`. Commit:
      `df-web: tracker connection and binding routes`.

## Task 6.4 — the console UI ✅

**Files:** `web/src/lib/types.ts`, `web/src/lib/api.ts`,
`web/src/routes/o/[org]/trackers/+page.svelte` (new),
`web/src/routes/trackers/callback/+page.svelte` (new),
`web/src/routes/o/[org]/repos/+page.svelte`, `web/src/routes/o/[org]/+layout.svelte`.

- [x] Types and `api.ts` methods for the seven endpoints, in the file's existing style.
- [x] The trackers page: one card per provider, `org.isAdmin` gating the buttons, the
      deployment's `configured` flag deciding between a connect button and an explanation.
- [x] The callback page: nonce check against `sessionStorage` before any `POST`, spinner,
      error, then `goto('/o/{org}/trackers')`. Runes only — `$state`/`$derived`/`$effect`.
- [x] The repos page: binding editor in the existing row expander, beside the leases.
- [x] Nav entry in the org layout.
- [x] `npm run check && npm run lint && npm test && npm run build`. Commit:
      `web: connect a tracker and bind a repo to it`.

## Task 6.5 — drop `trackerBinding` from the console ⬜

**Files:** `crates/df-web/src/routes/repos.rs`, `crates/df-web/src/openapi.rs`,
`web/src/lib/types.ts`, `crates/df-web/tests/console.rs`.

**This is the task's one breaking change** (spec §7). It lands alone so it can be reverted
without reverting the feature.

- [ ] Remove `tracker_binding` from `RegisterRepoRequest` and `UpdateRepoRequest`, passing
      `None` to `NewRepo`/`RepoPatch`. `df-core` keeps the field; the MCP surface is untouched.
- [ ] Remove it from the `RegisterRepoRequest`/`UpdateRepoRequest` components in
      `openapi.rs`; mark it `deprecated: true` on the `Repo` response component with a
      description naming `/api/orgs/{org}/repos/{repo}/tracker-binding` as the replacement.
- [ ] Remove it from `Repo` in `web/src/lib/types.ts` (nothing in `web/src` reads it).
- [ ] A test asserting a `POST /repos` carrying `trackerBinding` is accepted and *ignores*
      the field, rather than `400`ing — an unknown field is not an error in this API and
      making it one would be a second breaking change.
- [ ] `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`,
      `cargo fmt --all`, `npm run check && npm run lint && npm test`. Commit:
      `df-web: drop the free-form trackerBinding field from the console API`.

## Out-of-band reminders

- `.env.example` gains three vars (6.1) — confirm each says *why*.
- No migration, no new tenant table, no new MCP tool: nothing required in `0007_rls.sql`,
  `df-billing::classify`, `every_tool_has_a_price`, or `exhaustive_over`. State this
  explicitly in each PR body rather than leaving a reviewer to check.
- `Dockerfile`, `fly.toml`, `web/worker/`, `.github/workflows/` are untouched.
