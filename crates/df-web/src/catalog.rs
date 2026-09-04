//! The endpoint catalog — one description of the console API, used twice.
//!
//! **Why a catalog rather than annotations on each handler.** The plan asks for
//! an OpenAPI document generated from the handlers, and the failure mode it is
//! guarding against is a document that drifts from the code. A macro on each
//! handler is one way; this is another, and it makes drift impossible rather
//! than merely detectable: [`crate::router`] is *built from* this list, and
//! [`crate::openapi::document`] is *rendered from* the same list. There is no
//! second place where a route is declared, so there is nothing for a document
//! to fall out of step with.
//!
//! It also keeps the OpenAPI vocabulary out of `df-core`. The alternative —
//! deriving schema traits on the domain types — spreads a documentation
//! dependency across every crate to describe an interface only this one serves.
//!
//! The cost is honest and worth naming: request and response bodies are
//! referenced by component name rather than derived from the Rust types, so a
//! field added to a response struct does not appear in the document until
//! someone adds it to [`crate::openapi::components`]. The test at the bottom of
//! `openapi.rs` catches a *missing* component, not a stale field.

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put, MethodRouter};

use crate::routes::{auth, jobs, orgs, repos, teams, tokens, trackers, usage, webhooks};
use crate::state::AppState;
use crate::{oauth, openapi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "get",
            Verb::Post => "post",
            Verb::Put => "put",
            Verb::Patch => "patch",
            Verb::Delete => "delete",
        }
    }
}

/// What a caller must hold to reach an endpoint.
///
/// Documentation *and* an assertion: the test in `openapi.rs` checks that every
/// endpoint claiming an org scope actually sits under an `{org}` path segment,
/// which is what makes [`crate::session::OrgCtx`] able to resolve one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credential. Discovery documents, and the endpoints someone who cannot
    /// yet log in has to be able to reach.
    Public,
    /// A console session cookie.
    Session,
    /// A session, plus membership of the org in the path.
    OrgMember,
    /// A session, plus `owner` or `admin` of the org in the path.
    OrgAdmin,
}

impl Auth {
    pub fn as_str(self) -> &'static str {
        match self {
            Auth::Public => "public",
            Auth::Session => "session",
            Auth::OrgMember => "org member",
            Auth::OrgAdmin => "org admin",
        }
    }

    pub fn needs_org(self) -> bool {
        matches!(self, Auth::OrgMember | Auth::OrgAdmin)
    }
}

pub struct Endpoint {
    pub verb: Verb,
    pub path: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub auth: Auth,
    /// Component schema name for the request body, if any.
    pub request: Option<&'static str>,
    /// Component schema name for the response body, if any.
    pub response: Option<&'static str>,
    /// The success status this endpoint actually answers with. Defaults to
    /// `200`; call [`Endpoint::status`] for anything else. Getting this wrong
    /// is not cosmetic — a generated client that expects `200` treats a real
    /// `201`/`204`/`303` success as an error.
    pub success: u16,
    pub route: MethodRouter<AppState>,
}

impl Endpoint {
    fn build(verb: Verb, path: &'static str, route: MethodRouter<AppState>) -> Self {
        Self {
            verb,
            path,
            summary: "",
            description: "",
            auth: Auth::Session,
            request: None,
            response: None,
            success: 200,
            route,
        }
    }

    pub fn get<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Get, path, get(handler))
    }

    pub fn post<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Post, path, post(handler))
    }

    pub fn put<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Put, path, put(handler))
    }

    pub fn patch<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Patch, path, patch(handler))
    }

    pub fn delete<H, T>(path: &'static str, handler: H) -> Self
    where
        H: axum::handler::Handler<T, AppState>,
        T: 'static,
    {
        Self::build(Verb::Delete, path, delete(handler))
    }

    pub fn summary(mut self, summary: &'static str) -> Self {
        self.summary = summary;
        self
    }

    pub fn describe(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn takes(mut self, schema: &'static str) -> Self {
        self.request = Some(schema);
        self
    }

    pub fn returns(mut self, schema: &'static str) -> Self {
        self.response = Some(schema);
        self
    }

    /// Declare a success status other than the `200` default —
    /// `201 Created`, `204 No Content`, `303 See Other`, and so on, matching
    /// exactly what the handler actually sends.
    pub fn status(mut self, code: u16) -> Self {
        self.success = code;
        self
    }

    /// Cap the request body this route will read into memory, in bytes.
    ///
    /// Only needed on `Auth::Public` routes: session/token-authenticated
    /// endpoints already require a caller who spent a credential to reach
    /// them, but a public route like `/webhooks/{provider}` is reachable by
    /// anyone on the internet, and `Bytes`/`Json` extractors buffer the whole
    /// body before a handler ever runs. Without an explicit limit, an
    /// oversized payload is a free memory/CPU DoS against an unauthenticated
    /// surface.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.route = self.route.layer(DefaultBodyLimit::max(bytes));
        self
    }

    /// `GET /api/orgs/{org}/repos` → `getApiOrgsOrgRepos`. Stable across
    /// renames of the Rust function, which is what an OpenAPI operation id has
    /// to be — client generators turn it into a method name.
    pub fn operation_id(&self) -> String {
        let mut id = String::from(self.verb.as_str());
        for segment in self.path.split('/').filter(|s| !s.is_empty()) {
            let segment = segment.trim_matches(|c| c == '{' || c == '}');
            // Filtered before the first character is singled out, not after:
            // a segment like `.well-known` would otherwise capitalize the
            // leading `.` and leave it in place, producing an identifier that
            // starts with a dot. `operationId` becomes a method name in most
            // generators, and a leading `.` is not a legal identifier start
            // anywhere that matters.
            let mut chars = segment.chars().filter(|c| c.is_ascii_alphanumeric());
            if let Some(first) = chars.next() {
                id.push(first.to_ascii_uppercase());
                id.extend(chars);
            }
        }
        id
    }

    /// The `{name}` segments in this endpoint's path.
    pub fn path_params(&self) -> Vec<&'static str> {
        self.path
            .split('/')
            .filter(|s| s.starts_with('{') && s.ends_with('}'))
            .map(|s| s.trim_matches(|c| c == '{' || c == '}'))
            .collect()
    }
}

/// Every endpoint the console API serves.
pub fn catalog() -> Vec<Endpoint> {
    vec![
        // ------------------------------------------------------ discovery
        Endpoint::get(
            "/.well-known/oauth-authorization-server",
            oauth::as_metadata,
        )
        .auth(Auth::Public)
        .summary("Authorization server metadata")
        .describe(
            "RFC 8414. How an MCP client learns where to register, authorize, and \
                 exchange tokens. Open by necessity — it is what an unauthenticated \
                 client reads to find out how to authenticate.",
        ),
        Endpoint::get(
            "/.well-known/oauth-protected-resource",
            oauth::protected_resource_metadata,
        )
        .auth(Auth::Public)
        .summary("Protected resource metadata")
        .describe(
            "RFC 9728. Also served by the MCP surface, which is where its 401 \
             challenge points; served here too because some clients look for it \
             beside the authorization server's own document.",
        ),
        Endpoint::get("/api/openapi.json", openapi::serve)
            .auth(Auth::Public)
            .summary("This document")
            .describe("The OpenAPI description of everything below."),
        Endpoint::post("/webhooks/{provider}", webhooks::receive)
            .auth(Auth::Public)
            .body_limit(1_048_576)
            .summary("Receive tracker webhooks")
            .describe(
                "Public by necessity: GitHub App deliveries are authenticated by \
                 `X-Hub-Signature-256`, and JIRA Automation deliveries by the \
                 org's own `X-DF-Webhook-Secret` plus a `?site=<cloud-id>` URL \
                 parameter. This endpoint only verifies, parses, and resolves the \
                 owning org today; Task 4 turns accepted events into sync work. \
                 Bodies are capped at 1 MiB — far larger than any GitHub issue/\
                 comment or JIRA Automation payload this route parses, and small \
                 enough to bound memory/CPU spent buffering an unauthenticated \
                 request before signature verification runs.",
            ),
        // ----------------------------------------------------------- oauth
        Endpoint::post("/oauth/register", oauth::register_client)
            .auth(Auth::Public)
            .status(201)
            .summary("Register a client")
            .describe(
                "RFC 7591 dynamic client registration. Open by design — MCP clients \
                 self-register — and rate limited per source address. Registration \
                 grants nothing: a client is inert until a human consents to it.",
            ),
        Endpoint::get("/oauth/authorize", oauth::authorize_page)
            .auth(Auth::Public)
            .summary("Consent screen")
            .describe(
                "Renders the consent screen for an authorization request, or redirects \
                 to the login page with `next` set so the flow resumes afterwards. \
                 Reached by a top-level browser navigation, which is why the session \
                 cookie is SameSite=Lax.",
            ),
        Endpoint::post("/oauth/authorize", oauth::authorize_decision)
            .auth(Auth::Session)
            .status(303)
            .summary("Record the consent decision")
            .describe(
                "On approval, issues an authorization code and redirects to the \
                 client's callback. The selected org must be one the caller belongs \
                 to — this is where a token's org is fixed, and it cannot be changed \
                 afterwards.",
            ),
        Endpoint::post("/oauth/token", oauth::token)
            .auth(Auth::Public)
            .summary("Exchange a code or refresh token")
            .describe(
                "Form-encoded, per RFC 6749. `authorization_code` requires a PKCE \
                 S256 `code_verifier`; `refresh_token` rotates, and replaying a \
                 consumed refresh token revokes the whole chain.",
            ),
        Endpoint::post("/oauth/revoke", oauth::revoke)
            .auth(Auth::Public)
            .summary("Revoke a token")
            .describe("RFC 7009. Always 200, even for a token that never existed."),
        // ------------------------------------------------------------ auth
        Endpoint::post("/api/auth/signup/start", auth::signup_start)
            .auth(Auth::Public)
            .returns("RegistrationChallenge")
            .summary("Create an account and get a passkey challenge")
            .describe(
                "Takes no body — there is no identifier to give. Creates an account \
                 with no address and returns a WebAuthn creation challenge; it \
                 becomes usable only when a credential is registered against it at \
                 /api/auth/signup/finish. Because nothing is submitted, nothing here \
                 can reveal whether an address or account already exists.",
            ),
        Endpoint::post("/api/auth/signup/finish", auth::signup_finish)
            .auth(Auth::Public)
            .takes("FinishRegistration")
            .returns("SessionOpened")
            .summary("Register the passkey and open the first session"),
        Endpoint::post("/api/auth/login/start", auth::login_start)
            .auth(Auth::Public)
            .returns("AuthenticationChallenge")
            .summary("Get a sign-in challenge")
            .describe(
                "Takes no identifier: the credential the browser picks is what says \
                 who is signing in. allowCredentials is empty, so only discoverable \
                 passkeys answer it.",
            ),
        Endpoint::post("/api/auth/login/finish", auth::login_finish)
            .auth(Auth::Public)
            .takes("FinishAuthentication")
            .returns("SessionOpened")
            .summary("Present the signature and sign in")
            .describe(
                "Failures are one answer whatever went wrong — unknown credential, \
                 bad signature, wrong origin, disabled account.",
            ),
        Endpoint::post("/api/auth/claim/start", auth::claim_start)
            .auth(Auth::Public)
            .takes("ClaimRequest")
            .returns("RegistrationChallenge")
            .summary("Begin re-registering with an admin-issued claim code")
            .describe(
                "For an account whose passkeys an admin cleared. The code is not \
                 spent here, so an interrupted ceremony does not burn somebody's \
                 only way back in.",
            ),
        Endpoint::post("/api/auth/claim/finish", auth::claim_finish)
            .auth(Auth::Public)
            .takes("FinishClaim")
            .returns("SessionOpened")
            .summary("Register the new passkey and sign in")
            .describe("Spends the claim code."),
        Endpoint::post("/api/auth/logout", auth::logout)
            .auth(Auth::Public)
            .status(204)
            .summary("End this session")
            .describe("Succeeds even for a caller holding a cookie that resolves to nothing."),
        // -------------------------------------------------------------- me
        Endpoint::get("/api/me", auth::me)
            .returns("Me")
            .summary("Who is signed in")
            .describe("The account, its org memberships, and whether it still needs to enrol."),
        Endpoint::get("/api/me/sessions", auth::list_sessions)
            .returns("SessionList")
            .summary("Where this account is signed in"),
        Endpoint::delete("/api/me/sessions", auth::revoke_all_sessions)
            .summary("Sign out everywhere")
            .describe(
                "Ends every browser session, including this one. Leaves access tokens \
                 alone — those are a separate credential with their own revocation.",
            ),
        Endpoint::post("/api/me/passkeys/start", auth::add_passkey_start)
            .returns("RegistrationChallenge")
            .summary("Challenge to add another authenticator")
            .describe(
                "One passkey is one device, and there is no email to recover \
                 through. A second is the recovery story.",
            ),
        Endpoint::post("/api/me/passkeys/finish", auth::add_passkey_finish)
            .takes("FinishRegistration")
            .status(204)
            .summary("Register the additional authenticator"),
        Endpoint::get("/api/me/passkeys", auth::list_passkeys)
            .returns("PasskeyList")
            .summary("Authenticators registered to this account"),
        Endpoint::delete("/api/me/passkeys/{id}", auth::remove_passkey)
            .status(204)
            .summary("Remove an authenticator")
            .describe(
                "Refuses to remove the last one: that would lock the account out \
                 permanently, and the click looks like tidying up.",
            ),
        Endpoint::patch("/api/me/passkeys/{id}", auth::rename_passkey)
            .takes("RenameKeyRequest")
            .status(204)
            .summary("Name an authenticator"),
        Endpoint::patch("/api/me", auth::set_profile)
            .takes("ProfileRequest")
            .returns("User")
            .summary("Set the address and display name")
            .describe(
                "The one endpoint that will say an address is already in use. It \
                 needs a session, which makes that answer attributable and \
                 rate-limited rather than something a stranger can walk a list \
                 against — which is why the address is set here and not at signup.",
            ),
        // ------------------------------------------------------------ orgs
        Endpoint::get("/api/orgs", orgs::list_orgs)
            .returns("MembershipList")
            .summary("Orgs this account belongs to"),
        Endpoint::post("/api/orgs", orgs::create_org)
            .takes("CreateOrgRequest")
            .returns("Org")
            .status(201)
            .summary("Create an org")
            .describe("The creator becomes its owner. Requires a registered passkey."),
        Endpoint::get("/api/orgs/{org}", orgs::get_org)
            .auth(Auth::OrgMember)
            .returns("Joined")
            .summary("One org, with your role in it"),
        Endpoint::get("/api/orgs/{org}/members", orgs::list_members)
            .auth(Auth::OrgMember)
            .returns("OrgMemberList")
            .summary("Everyone in the org")
            .describe("Open to any member: who else is in your own org is not privileged."),
        Endpoint::patch("/api/orgs/{org}/members/{user}", orgs::set_member_role)
            .auth(Auth::OrgAdmin)
            .takes("RoleRequest")
            .status(204)
            .summary("Change someone's role")
            .describe(
                "Only an owner may create or demote another owner, and the last owner \
                 cannot be demoted.",
            ),
        Endpoint::post(
            "/api/orgs/{org}/members/{user}/reset-passkeys",
            orgs::reset_member_passkeys,
        )
        .auth(Auth::OrgAdmin)
        .returns("ClaimCode")
        .status(201)
        .summary("Clear a member's passkeys and issue a re-registration code")
        .describe(
            "The only assisted account recovery there is. Clears every passkey, ends \
             every session, and returns a one-time code the admin hands over — the \
             code is what stops the account being claimable by whoever reaches \
             registration first. Only an owner may reset an owner.",
        ),
        Endpoint::delete("/api/orgs/{org}/members/{user}", orgs::remove_member)
            .auth(Auth::OrgAdmin)
            .status(204)
            .summary("Remove a member")
            .describe(
                "Also clears their team memberships and revokes the tokens they held \
                 in this org. Removing yourself needs no privilege; the last owner \
                 cannot be removed.",
            ),
        Endpoint::post("/api/orgs/{org}/members/{user}/logout", orgs::force_logout)
            .auth(Auth::OrgAdmin)
            .summary("Force a member to sign out")
            .describe(
                "Ends every browser session that user holds. For a lost laptop — it \
             leaves membership and tokens alone.",
            ),
        // --------------------------------------------------------- invites
        Endpoint::get("/api/orgs/{org}/invites", orgs::list_invites)
            .auth(Auth::OrgAdmin)
            .returns("InviteList")
            .summary("Outstanding invitations"),
        Endpoint::post("/api/orgs/{org}/invites", orgs::create_invite)
            .auth(Auth::OrgAdmin)
            .takes("InviteRequest")
            .returns("CreatedInvite")
            .status(201)
            .summary("Invite someone by email address")
            .describe(
                "Returns a single-use code, good for 14 days, which the admin \
                 delivers however they like — nothing is emailed. The code is shown \
                 only in this response and cannot be read back; only its hash is \
                 stored. Supersedes any live invitation for the same address. Only an \
                 owner may invite an owner. Redeemable only by someone signed in as \
                 the address invited, so a leaked code is not a free seat.",
            ),
        Endpoint::delete("/api/orgs/{org}/invites/{id}", orgs::revoke_invite)
            .auth(Auth::OrgAdmin)
            .status(204)
            .summary("Withdraw an invitation"),
        Endpoint::post("/api/orgs/{org}/invites/accept", orgs::accept_invite)
            .takes("AcceptInviteRequest")
            .returns("Joined")
            .summary("Accept an invitation")
            .describe(
                "Requires a session whose verified address matches the one invited — \
                 otherwise a forwarded invitation mail is a way into someone else's \
                 org. POST, like every other credential redemption here.",
            ),
        // ----------------------------------------------------------- teams
        Endpoint::get("/api/orgs/{org}/teams", teams::list_teams)
            .auth(Auth::OrgMember)
            .returns("TeamList")
            .summary("Teams in this org"),
        Endpoint::post("/api/orgs/{org}/teams", teams::create_team)
            .auth(Auth::OrgAdmin)
            .takes("CreateTeamRequest")
            .returns("Team")
            .status(201)
            .summary("Create a team"),
        Endpoint::get("/api/orgs/{org}/teams/{team}", teams::get_team)
            .auth(Auth::OrgMember)
            .returns("Team")
            .summary("One team, by slug"),
        Endpoint::patch("/api/orgs/{org}/teams/{team}", teams::update_team)
            .auth(Auth::OrgAdmin)
            .takes("TeamPatch")
            .returns("Team")
            .summary("Rename a team"),
        Endpoint::delete("/api/orgs/{org}/teams/{team}", teams::delete_team)
            .auth(Auth::OrgAdmin)
            .status(204)
            .summary("Delete a team")
            .describe(
                "Refused while repos are still scoped to it: a null team means \
                 org-wide, so deleting would quietly publish them to everyone.",
            ),
        Endpoint::get(
            "/api/orgs/{org}/teams/{team}/members",
            teams::list_team_members,
        )
        .auth(Auth::OrgMember)
        .returns("TeamMemberList")
        .summary("Who is on a team"),
        Endpoint::put(
            "/api/orgs/{org}/teams/{team}/members/{user}",
            teams::add_team_member,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Put a member on a team")
        .describe("Idempotent. The user must already be a member of the org."),
        Endpoint::delete(
            "/api/orgs/{org}/teams/{team}/members/{user}",
            teams::remove_team_member,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Take a member off a team"),
        // ----------------------------------------------------------- repos
        Endpoint::get("/api/orgs/{org}/repos", repos::list_repos)
            .auth(Auth::OrgMember)
            .returns("RepoList")
            .summary("Registered repos")
            .describe("Pass `includeInactive=true` to include soft-disabled ones."),
        Endpoint::post("/api/orgs/{org}/repos", repos::register_repo)
            .auth(Auth::OrgAdmin)
            .takes("RegisterRepoRequest")
            .returns("Repo")
            .status(201)
            .summary("Register a repo")
            .describe(
                "Remotes are normalized, so the SSH and HTTPS forms of one repository \
                 collapse to a single row and either resolves to it.",
            ),
        Endpoint::get("/api/orgs/{org}/repos/{repo}", repos::get_repo)
            .auth(Auth::OrgMember)
            .returns("Repo")
            .summary("One repo, by slug"),
        Endpoint::patch("/api/orgs/{org}/repos/{repo}", repos::update_repo)
            .auth(Auth::OrgAdmin)
            .takes("UpdateRepoRequest")
            .returns("Repo")
            .summary("Update a repo")
            .describe(
                "Absent fields are left alone. The slug cannot be changed — agents \
                 name it, so renaming would break every skill that does.",
            ),
        // -------------------------------------------------------- trackers
        Endpoint::get(
            "/api/orgs/{org}/tracker-connections",
            trackers::list_tracker_connections,
        )
        .auth(Auth::OrgAdmin)
        .returns("TrackerConnections")
        .summary("Tracker connections, and what this deployment can connect")
        .describe(
            "Stored ciphertext is never returned; `hasCredentials` says whether there is \
             any. The per-provider `configured` flag is the conjunction of every credential \
             the connect flow needs, so a console never offers a flow the server cannot \
             finish.",
        ),
        Endpoint::post(
            "/api/orgs/{org}/tracker-connections/{provider}",
            trackers::connect_tracker,
        )
        .auth(Auth::OrgAdmin)
        .takes("ConnectTrackerRequest")
        .returns("TrackerConnection")
        .summary("Redeem a provider authorization and record the connection")
        .describe(
            "A POST because the body carries a single-use authorization code: a link \
             preview following the provider's redirect must burn nothing. GitHub also \
             requires `installationId`, and it is verified against the installations the \
             authorizing account actually administers — an unverified installation id \
             would let one org drive another's issues.",
        ),
        Endpoint::delete(
            "/api/orgs/{org}/tracker-connections/{provider}",
            trackers::disconnect_tracker,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Disconnect a tracker")
        .describe(
            "Bindings pointing at it go inert rather than invalid. Removing the connection \
             also releases the provider's external id, so another org can connect it.",
        ),
        Endpoint::get(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings",
            trackers::list_repo_bindings,
        )
        .auth(Auth::OrgMember)
        .returns("TrackerBindingList")
        .summary("Which tracker projects this repo maps to")
        .describe("`live` is false while the org has no connection for that provider."),
        Endpoint::put(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}",
            trackers::bind_repo,
        )
        .auth(Auth::OrgAdmin)
        .takes("BindRepoRequest")
        .returns("TrackerBinding")
        .summary("Point a repo at a tracker project")
        .describe(
            "`externalRef` is `owner/repo` for GitHub and a project key for JIRA — the \
             exact strings webhook ingest matches on, so anything else is refused rather \
             than stored inert. The connection is looked up from the org, never supplied.",
        ),
        Endpoint::delete(
            "/api/orgs/{org}/repos/{repo}/tracker-bindings/{provider}",
            trackers::unbind_repo,
        )
        .auth(Auth::OrgAdmin)
        .status(204)
        .summary("Stop mapping a repo to a tracker project"),
        Endpoint::get("/api/orgs/{org}/repos/{repo}/leases", repos::list_leases)
            .auth(Auth::OrgMember)
            .returns("LeaseList")
            .summary("Who is in this repo right now")
            .describe("The console's answer to \"why is my agent waiting?\"."),
        // ----------------------------------------------------------- queue
        Endpoint::get("/api/orgs/{org}/jobs", jobs::list_jobs)
            .auth(Auth::OrgMember)
            .returns("JobList")
            .summary("The queue, newest first")
            .describe(
                "Read-only, and deliberately so: every write to a job belongs to the \
                 MCP surface, because the agent doing the work is the only party that \
                 can say when it is done. Filter with `status`, `repo`, `team`, and \
                 `mine=true`; `repo` and `team` name slugs, and an unregistered one \
                 is an error listing what is registered.",
            ),
        Endpoint::get("/api/orgs/{org}/jobs/stats", jobs::job_stats)
            .auth(Auth::OrgMember)
            .returns("QueueStats")
            .summary("Counts per status, plus how many are blocked")
            .describe(
                "`blocked` is not a status — it counts pending jobs still waiting on \
                 a dependency, which is what separates a queue that is idle from one \
                 that is stuck. Optionally narrowed to one repo slug.",
            ),
        Endpoint::get("/api/orgs/{org}/jobs/{job}", jobs::get_job)
            .auth(Auth::OrgMember)
            .returns("JobDetail")
            .summary("One job, with the jobs it waits on"),
        // -------------------------------------------------- tokens & usage
        Endpoint::get("/api/orgs/{org}/tokens", tokens::list_tokens)
            .auth(Auth::OrgMember)
            .returns("TokenSummaryList")
            .summary("Your live tokens in this org")
            .describe("Yours only, OAuth tokens included, so you can cut off any agent."),
        Endpoint::post("/api/orgs/{org}/tokens", tokens::mint_token)
            .auth(Auth::OrgMember)
            .takes("MintTokenRequest")
            .returns("MintedToken")
            .status(201)
            .summary("Mint a personal access token")
            .describe(
                "The compatibility path for clients whose OAuth support is partial. \
                 Shown once. You can only mint your own, and only with scopes you \
                 hold.",
            ),
        Endpoint::delete("/api/orgs/{org}/tokens/{id}", tokens::revoke_token)
            .auth(Auth::OrgMember)
            .status(204)
            .summary("Revoke one of your tokens")
            .describe("Takes effect on the agent's next call, not at some later expiry."),
        Endpoint::get("/api/orgs/{org}/usage", usage::get_usage)
            .auth(Auth::OrgMember)
            .returns("UsageStatus")
            .summary("This period's usage against the plan")
            .describe("Free to read, and readable by an org that has run out."),
        Endpoint::get("/api/orgs/{org}/audit", usage::get_audit)
            .auth(Auth::OrgAdmin)
            .returns("AuditEventList")
            .summary("The org's security log")
            .describe(
                "Admin-only, unlike the rest of the console's reads: membership \
                 changes and failed logins are what an attacker with a low-privilege \
                 session would read before choosing a target.",
            ),
    ]
}
