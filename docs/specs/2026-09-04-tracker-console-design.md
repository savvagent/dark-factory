# Tracker console design — Milestone 2 Task 6

**Parent spec:** [`docs/specs/2026-09-03-df-trackers-design.md`](2026-09-03-df-trackers-design.md).
**Plan:** [`docs/plans/2026-09-04-tracker-console.md`](../plans/2026-09-04-tracker-console.md).

Task 6 is the last task in Milestone 2: the human-facing half of the tracker
integration. Tasks 1–5 built the schema, the provider clients, webhook ingest, the sync
engine, and the `link_ticket`/`sync_ticket` MCP tools — and every one of them reads
`tracker_connections` and `tracker_bindings` rows that, today, **nothing can create**.
There is no console page, no REST route, and no MCP tool that writes either table. The
sync engine is complete and unreachable.

## Goal & Success Criteria

An org admin can, in the console:

1. Connect their GitHub App installation to their org, and disconnect it.
2. Connect their JIRA cloud site to their org, and disconnect it.
3. Point a registered repo at a GitHub repository (`owner/repo`) or a JIRA project key,
   with the trigger label that inbound sync watches for, and remove that binding.

And, on the way through, the console stops carrying the vestigial free-form
`repos.tracker_binding` JSON blob.

Success is: a fresh org with a GitHub App installation and one bound repo receives an
inbound webhook for a labelled issue and gets a job, with no `psql` and no MCP call
anywhere in the setup path.

## Premise corrections

Two things the parent plan's four-checkbox sketch of Task 6 assumed that are not true of
the code as it stands.

- **"`df-web`'s `catalog.rs` if new read/write REST routes are needed" — they are needed,
  all of them.** The catalog has no tracker route at all beyond `POST /webhooks/{provider}`.
  Task 6 is not a UI task with an incidental route or two; it is a server task (a new
  `df-web::routes::trackers` module, six endpoints, new config) with a UI on top. The plan
  splits accordingly.

- **The connection rows cannot be created from an admin-supplied identifier alone.**
  Binding a GitHub installation means writing an `external_id` that
  `tracker_connection_index` then maps *globally* — `PRIMARY KEY (provider, external_id)`
  — to one org. An endpoint that took `{"installationId": 12345}` on an admin's word would
  let any org admin claim any installation id no other dark-factory org had claimed yet,
  and then drive comments and state transitions on that installation's issues using the
  operator's own App credentials. Installation ids are small sequential integers. This is a
  cross-tenant escalation reachable by typing a number, so the GitHub flow verifies
  possession (§2) rather than trusting the redirect. The parent spec did not consider it
  because until Task 6 nothing wrote a connection row.

## Scope

**In:**
- Six console REST endpoints (§3): list/create/delete a tracker connection; read/set/delete
  a repo's tracker binding.
- GitHub App installation binding with user-to-server verification (§2).
- JIRA 3LO binding, sealing the refresh token into `encrypted_credentials` (§2).
- New deployment config: `DF_GITHUB_APP_SLUG`, `DF_GITHUB_APP_CLIENT_ID`,
  `DF_GITHUB_APP_CLIENT_SECRET` (§5).
- Console pages: an org-level `Trackers` page, a provider-agnostic OAuth return page, and a
  per-repo binding editor on the existing repos page (§6).
- Removing `trackerBinding` from the console's repo write surface and TypeScript types (§7).

**Out:**
- Removing the `repos.tracker_binding` column from the database, or from the `Repo` body the
  MCP tools return. Dropping a column is forward-only-migration surgery with an MCP-visible
  blast radius; deprecating the console's use of it is the reversible half and the half that
  was asked for. §7 draws the line precisely.
- More than one connection per provider per org. Unchanged v1 simplification from the parent
  spec, and the schema's `UNIQUE (org_id, provider)` still enforces it.
- Any new MCP tool. Tracker *setup* is an admin act performed by a human in a browser; the
  agent-facing surface (`link_ticket`, `sync_ticket`) shipped in Task 5 and is unchanged.
  Nothing here needs `df-billing::classify`.
- A conflict/merge UI, a webhook replay viewer, connection health checks. Not asked for.

## §1 Where a connection comes from

Both providers hand the browser a one-time authorization artifact and expect the server to
redeem it. That shapes everything else:

- The redemption endpoint is a **`POST`**, never a `GET`, per the console's standing rule.
  A link-preview fetcher that follows an OAuth return URL must burn nothing.
- The provider redirect therefore lands on a console **page**, which reads the query string
  and `POST`s it. That page is `/trackers/callback` — **not** under `/o/[org]/`, because a
  provider redirect URI is one static string registered with the provider, and it cannot
  contain an org slug that varies per customer.
- The org therefore travels in the OAuth `state` parameter, along with a nonce the console
  minted and stashed in `sessionStorage`. The callback page refuses to `POST` if the
  returned `state` does not match what it stored — the standard login-CSRF guard, which
  here prevents an attacker's authorization code being bound into a victim admin's org.
  `state` is *not* server-side state: the redemption `POST` is already session-authenticated
  and `require_admin`-gated, so the nonce guards the one thing the session cannot.

## §2 Proving the admin actually holds what they are binding

**GitHub.** The App is configured with "Request user authorization (OAuth) during
installation", so GitHub's post-install redirect carries both `installation_id` and a
user-authorization `code`. The server exchanges that code for a user-to-server token
(`POST https://github.com/login/oauth/access_token`, the App's client id and secret) and
calls `GET /user/installations`. The claimed installation id must appear in the result, or
the request is refused and nothing is written. This is possession, not assertion: the token
speaks for the human who just clicked through GitHub's own install screen, and GitHub —
not dark-factory — decides which installations that human can see.

`GET /app/installations/{id}` (an App-JWT call, which the existing client could already
make) is *not* sufficient and is not used: it proves the installation exists, which is
exactly what an attacker guessing sequential integers already assumes.

**JIRA.** The 3LO code exchange is itself the proof — Atlassian issued the code to this
browser after this human consented on this site, and `GET /oauth/token/accessible-resources`
with the resulting access token enumerates precisely the sites they granted. The site id
is read from that response, never from the request body.

If that response names more than one site, the server writes nothing and returns an error
listing the site names and ids, telling the admin to re-run the connect flow granting a
single site. dark-factory stores one JIRA site per org (`UNIQUE (org_id, provider)`), the
authorization code is single-use so there is no second round trip to ask with, and picking
one silently is precisely the "errors that guess" this codebase refuses. Accepted as a rare,
loud dead end rather than solved with server-side pending-token storage.

The refresh token from the exchange is sealed with `df_core::crypto::Cipher` and stored in
`encrypted_credentials`, in the same `base64(nonce || ciphertext)` encoding
`df_core::trackers::decode_stored_secret` reads — the encoding `df-mcp`'s `sync_ticket`
already round-trips when it rotates the token.

## §3 The endpoints

All six are `Auth::OrgAdmin`. Tracker setup grants a repo the ability to move a customer's
tickets; it is an owner/admin act, and `OrgCtx::require_admin` is the extractor that says so
before any handler body runs.

| Verb | Path | Body | Answers |
|---|---|---|---|
| `GET` | `/api/orgs/{org}/tracker-connections` | — | `TrackerConnectionsView` |
| `POST` | `/api/orgs/{org}/tracker-connections/github` | `{installationId, code}` | `TrackerConnectionView` |
| `POST` | `/api/orgs/{org}/tracker-connections/jira` | `{code}` | `TrackerConnectionView` |
| `DELETE` | `/api/orgs/{org}/tracker-connections/{provider}` | — | `204` |
| `PUT` | `/api/orgs/{org}/repos/{repo}/tracker-binding` | `{provider, externalRef, triggerLabel?}` | `TrackerBindingView` |
| `DELETE` | `/api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}` | — | `204` |

A repo's bindings are read from `GET /api/orgs/{org}/repos/{repo}/tracker-bindings`
(`Auth::OrgMember`, since it is a read of repo metadata a member may already see) — seven
endpoints in total, six of them admin-only.

**`TrackerConnectionView` is a `df-web` type, not `df_core::TrackerConnection`.** The domain
row carries `encrypted_credentials` and `encrypted_webhook_secret` and `#[derive(Serialize)]`
would put both on the wire. Ciphertext is not a secret in the sense that leaking it grants
access, but a console `GET` that returns a sealed refresh token to every admin's browser is
gratuitous exposure of the exact material `DF_ENCRYPTION_KEY` exists to protect. The view is
`{id, provider, externalId, hasCredentials, createdAt, updatedAt}`, and a unit test asserts
no serialization of it contains `"encrypted"`.

`TrackerConnectionsView` wraps the list with what the deployment supports:
`{connections: [...], github: {configured, installUrl}, jira: {configured, authorizeUrl}}`.
The two URLs are built **server-side** from the App slug / client id and `DF_PUBLIC_URL`,
and the console appends only `&state=`. Nothing about the deployment is baked into the
bundle — a hard-coded App slug is how a staging console sends admins to install the
production App. `configured: false` (the operator set no GitHub or no JIRA credentials) is
what the page renders as "this deployment does not offer JIRA sync", instead of a button
that leads to a 500.

**A binding is auto-linked to the org's connection.** `PUT …/tracker-binding` looks up the
org's connection for the named provider and passes its id as `connection_id`, rather than
taking one from the request. A binding pointing at another provider's connection is not a
shape any caller should be able to construct, and the parent spec's "a repo may declare a
binding before a connection exists" case is preserved by passing `None` when there is none —
the row is written, inert, and becomes live when the connection is made.

**`external_ref` is validated per provider**, because a typo here is silent: a GitHub
binding must be `owner/repo` (two non-empty, slash-free segments) because that is what
`webhook.rs` matches `repository.full_name` against, and a JIRA binding must be a project
key (the shape `validate_jira_issue_key` already knows) because that is what it matches
`fields.project.key` against. A binding that can never match an inbound event is a
configuration error worth catching at the point of typing, not at 3am when the issue label
appears to do nothing.

## §4 Tenant isolation

No new table, so no new RLS policy and no new entry in `0007_rls.sql`'s array. Both tables
already carry `<table>_tenant_isolation` policies from `0011_trackers.sql` and cross-org
negative tests from Task 1.

What Task 6 adds is a second *surface* onto those tables, so guard 1 is what needs asserting:
every new handler goes through `state.db.begin(ctx.org.id)`, every `df-core` function it
calls already binds `org_id = $1`, and the console tests assert an admin of org A gets `404`
— never `403` — for org B's tracker routes.

The two new `df-core` functions (`list_connections`, `list_bindings_for_repo`) take `&mut Tx`
like every other one in the module, and get cross-org tests in `crates/df-core/tests/trackers.rs`.

## §5 Config

Three new optional vars, following `github_app_id`'s existing shape exactly — optional
because a deployment that offers no GitHub integration has no App:

- `DF_GITHUB_APP_SLUG` — the App's URL slug, for `https://github.com/apps/{slug}/installations/new`.
- `DF_GITHUB_APP_CLIENT_ID`, `DF_GITHUB_APP_CLIENT_SECRET` — the App's OAuth credentials,
  for the user-to-server exchange in §2.

`DF_JIRA_CLIENT_ID`/`DF_JIRA_CLIENT_SECRET` already exist and are already threaded to
`df-mcp`; Task 6 threads them to `df-web` as well.

None of them parse to anything but a string, so there is no unparseable-value case to fail
loudly on — but the *combination* has one: a deployment with an App id and private key but
no client id can mint tokens and receive webhooks while being unable to connect anybody.
`configured` in §3's response is the conjunction, computed in one place, so the console
never offers a flow the server cannot finish.

## §6 The console

- **`/o/[org]/trackers`** — the connections page. Admin-only in the sense the rest of the
  console means it: `OrgContext.isAdmin` hides the buttons, `OrgCtx::require_admin` refuses
  them. Renders one card per provider: connected (external id, when, disconnect) or not
  (a connect button, or an explanation that the deployment does not offer it).
- **`/trackers/callback`** — the provider return page. Not org-scoped (§1). Reads
  `installation_id`/`code`/`state`, checks the nonce against `sessionStorage`, `POST`s to the
  org-scoped endpoint, and navigates to `/o/{org}/trackers` with the result. It renders a
  spinner and an error, and nothing else.
- **The repos page** grows a per-repo binding editor inside the row expander that already
  exists for leases — provider, external ref, trigger label, and a remove button. A repo's
  binding belongs next to the repo, not on a page of its own; the parent spec called this
  "a repo settings addition, not a new page" and that still reads right.

## §7 Dropping `trackerBinding` from the console

The free-form `repos.tracker_binding jsonb` column (Milestone 1) and the structured
`tracker_bindings` table (Task 1) both answer "what tracker does this repo map to", and only
one of them is read by anything: the sync engine, webhook ingest, and both MCP tools read the
table. The column is read by nothing at all.

Task 6 removes it from the console, exactly:

- `RegisterRepoRequest` and `UpdateRepoRequest` lose the field, so the console API can no
  longer write the blob. `NewRepo`/`RepoPatch` in `df-core` keep it — the MCP tools still
  accept it, and this task is not a change to the agent-facing surface.
- `openapi.rs` loses it from those two request schemas and marks it `deprecated` on the
  `Repo` response schema, naming the tracker-binding endpoints as the replacement.
- `web/src/lib/types.ts` loses it from `Repo`. Nothing in `web/src` ever read it.

**This is a breaking change to the console REST API**, and it is named here rather than
discovered by a reviewer: a client that today `POST`s `{"slug": "x", "trackerBinding": {...}}`
to `/api/orgs/{org}/repos` will, after this change, have that field ignored rather than
stored. The blast radius is the console itself (which never sent it) and any direct API
consumer; the field is four days old, has no UI, and was superseded before it was used. The
column and its data are untouched, so the change is reversible by restoring two struct fields.

## Error Handling & Edge Cases

- **`code` already redeemed** (admin refreshes the callback page): both providers answer the
  exchange with an error; the handler returns `invalid` naming the provider and saying to
  re-run the connect flow. Nothing partial is written — the whole handler is one `Tx`.
- **Installation id not in `/user/installations`**: refused as `invalid`, saying the signed-in
  GitHub account does not administer that installation. Deliberately not `not_found`; the
  admin needs to know the difference between "no such thing" and "not yours".
- **Installation already claimed by another org**: `df-core`'s `upsert_connection_index`
  already raises `Error::Invalid` naming exactly that, and the transaction rolls back. The
  console renders the message unchanged; it is the truth and the admin's next step (ask the
  other org to disconnect) follows from it.
- **Disconnecting a connection with live bindings**: allowed. `tracker_bindings.connection_id`
  is `ON DELETE SET NULL` by design, and the parent spec's rule is that a binding with no
  connection is inert rather than invalid. The page says so where the disconnect button is,
  and `delete_connection` already removes the index row so another org can claim the
  installation afterwards.
- **A repo bound before its connection exists**: `connection_id` is `NULL`, the row is inert,
  and the page labels it "waiting for a JIRA connection". This is the parent spec's case,
  now reachable.
- **JIRA grant covering several sites**: §2. Loud dead end, no write.

## Risks & Open Questions

- **The GitHub verification depends on an App setting the operator must have enabled.** If
  "Request user authorization (OAuth) during installation" is off, GitHub's redirect carries
  no `code`, and the connect flow fails with a message saying so rather than falling back to
  trusting the installation id. Naming it in `.env.example` next to `DF_GITHUB_APP_CLIENT_ID`
  is the mitigation; there is no way for the server to detect the setting in advance.
- **`state` lives in `sessionStorage`, so a connect flow that finishes in a different tab or
  after a browser restart fails the nonce check** and asks the admin to start again. Correct
  but occasionally annoying. A server-side pending-authorization table would fix it and is not
  worth a table.
- **One JIRA site and one GitHub installation per org** remains the v1 shape, inherited
  unchanged. The multi-site error message in §2 is where a customer who needs more will make
  themselves known.
