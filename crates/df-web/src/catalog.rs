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

use axum::routing::{delete, get, patch, post, put, MethodRouter};

use crate::routes::{auth, orgs, repos, teams, tokens, usage};
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

    /// `GET /api/orgs/{org}/repos` → `getApiOrgsOrgRepos`. Stable across
    /// renames of the Rust function, which is what an OpenAPI operation id has
    /// to be — client generators turn it into a method name.
    pub fn operation_id(&self) -> String {
        let mut id = String::from(self.verb.as_str());
        for segment in self.path.split('/').filter(|s| !s.is_empty()) {
            let segment = segment.trim_matches(|c| c == '{' || c == '}');
            let mut chars = segment.chars();
            if let Some(first) = chars.next() {
                id.push(first.to_ascii_uppercase());
                id.extend(chars.filter(|c| c.is_ascii_alphanumeric()));
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
        // ----------------------------------------------------------- oauth
        Endpoint::post("/oauth/register", oauth::register_client)
            .auth(Auth::Public)
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
        Endpoint::post("/api/auth/signup", auth::signup)
            .auth(Auth::Public)
            .takes("SignupRequest")
            .returns("Accepted")
            .summary("Create an account")
            .describe(
                "Creates the user and mails a verification link. Answers identically \
                 whether or not the address is already registered.",
            ),
        Endpoint::post("/api/auth/link", auth::request_link)
            .auth(Auth::Public)
            .takes("LinkRequest")
            .returns("Accepted")
            .summary("Mail a verification or recovery link")
            .describe(
                "For someone whose verification mail never arrived, or whose \
                 authenticator is gone. Throttled per address — what is being \
                 protected is the recipient's mailbox.",
            ),
        Endpoint::post("/api/auth/verify", auth::verify)
            .auth(Auth::Public)
            .takes("TokenRequest")
            .returns("Verified")
            .summary("Spend a verification link")
            .describe(
                "POST, never GET: mail scanners follow every link they see, and a \
                 single-use GET is spent before the human clicks it. Opens a session \
                 only for an account that has no confirmed authenticator yet — which \
                 is what makes first enrollment reachable.",
            ),
        Endpoint::post("/api/auth/recover", auth::recover)
            .auth(Auth::Public)
            .takes("TokenRequest")
            .returns("SessionOpened")
            .summary("Spend a recovery link")
            .describe(
                "Removes the current authenticator and opens a session so a new one \
                 can be enrolled. POST, for the same reason as verification.",
            ),
        Endpoint::post("/api/auth/login", auth::login_totp)
            .auth(Auth::Public)
            .takes("LoginRequest")
            .returns("SessionOpened")
            .summary("Sign in with an authenticator code")
            .describe(
                "Email plus a six-digit code. Failures are constant-shape: unknown \
                 address, disabled account, wrong code, and replayed code are one \
                 answer.",
            ),
        Endpoint::post("/api/auth/login/recovery", auth::login_recovery_code)
            .auth(Auth::Public)
            .takes("LoginRequest")
            .returns("SessionOpened")
            .summary("Sign in with a recovery code")
            .describe(
                "Uses one of the codes issued at enrollment. Leaves TOTP intact — the \
                 user still holds the secret, they just cannot reach it right now.",
            ),
        Endpoint::post("/api/auth/logout", auth::logout)
            .auth(Auth::Public)
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
        Endpoint::post("/api/me/totp", auth::begin_totp)
            .returns("Enrollment")
            .summary("Start enrolling an authenticator")
            .describe(
                "Returns a provisioning URI for a QR code, a manual key, and ten \
                 single-use recovery codes. The recovery codes are shown once and \
                 stored only as hashes.",
            ),
        Endpoint::post("/api/me/totp/confirm", auth::confirm_totp)
            .takes("ConfirmTotpRequest")
            .summary("Finish enrollment")
            .describe("Proves possession. Until this succeeds the credential cannot sign in."),
        Endpoint::post("/api/me/recovery-codes", auth::reissue_recovery_codes)
            .returns("RecoveryCodes")
            .summary("Issue a fresh set of recovery codes")
            .describe("Invalidates the previous set. Shown once."),
        // ------------------------------------------------------------ orgs
        Endpoint::get("/api/orgs", orgs::list_orgs)
            .returns("MembershipList")
            .summary("Orgs this account belongs to"),
        Endpoint::post("/api/orgs", orgs::create_org)
            .takes("CreateOrgRequest")
            .returns("Org")
            .summary("Create an org")
            .describe("The creator becomes its owner. Requires a verified email address."),
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
            .summary("Change someone's role")
            .describe(
                "Only an owner may create or demote another owner, and the last owner \
                 cannot be demoted.",
            ),
        Endpoint::delete("/api/orgs/{org}/members/{user}", orgs::remove_member)
            .auth(Auth::OrgAdmin)
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
            .returns("Invite")
            .summary("Invite someone by email")
            .describe(
                "Mails a single-use link, good for 14 days. Supersedes any live \
                 invitation for the same address. Only an owner may invite an owner.",
            ),
        Endpoint::delete("/api/orgs/{org}/invites/{id}", orgs::revoke_invite)
            .auth(Auth::OrgAdmin)
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
        .summary("Put a member on a team")
        .describe("Idempotent. The user must already be a member of the org."),
        Endpoint::delete(
            "/api/orgs/{org}/teams/{team}/members/{user}",
            teams::remove_team_member,
        )
        .auth(Auth::OrgAdmin)
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
        Endpoint::get("/api/orgs/{org}/repos/{repo}/leases", repos::list_leases)
            .auth(Auth::OrgMember)
            .returns("LeaseList")
            .summary("Who is in this repo right now")
            .describe("The console's answer to \"why is my agent waiting?\"."),
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
            .summary("Mint a personal access token")
            .describe(
                "The compatibility path for clients whose OAuth support is partial. \
                 Shown once. You can only mint your own, and only with scopes you \
                 hold.",
            ),
        Endpoint::delete("/api/orgs/{org}/tokens/{id}", tokens::revoke_token)
            .auth(Auth::OrgMember)
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
