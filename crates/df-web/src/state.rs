//! Everything a console handler needs, and the configuration it cannot infer.

use std::sync::Arc;

use df_core::crypto::Cipher;
use df_core::Db;

/// What the JIRA connection asks Atlassian for.
///
/// Exactly what `df_trackers::jira::JiraClient` calls: reading an issue's
/// `updated` field and its transitions, writing a comment and a transition.
/// `offline_access` is what yields the refresh token the connection is stored
/// with. Asking for more than this would put a consent screen in front of an
/// admin listing permissions the product never uses.
const JIRA_SCOPES: &str = "read:jira-work write:jira-work offline_access";

/// Deployment-dependent settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Public base URL of the console and the authorization server — the origin
    /// a browser sees. Every link the product hands out is built from it: the
    /// OAuth issuer, both discovery documents, and the invitation URL an admin
    /// copies. A wrong value produces links that 404 rather than links that leak.
    pub public_url: String,

    /// Canonical URI of the MCP resource server. Tokens minted here — including
    /// personal access tokens — are audienced for exactly this, and `df-mcp`
    /// refuses anything else.
    ///
    /// Configuration rather than something derived from the request, for the
    /// same reason as in `df-mcp`: a `Host` header is attacker-controlled, and
    /// an audience derived from one is not an audience check.
    pub resource_uri: String,

    /// Unused since passkeys replaced TOTP — see `df_server::Config::totp_issuer`.
    pub totp_issuer: String,

    /// Shared secret for GitHub webhook signature verification. Optional
    /// because tracker integration itself is optional per deployment.
    pub github_app_webhook_secret: Option<String>,

    /// The GitHub App's URL slug, and its *user*-to-server OAuth credentials.
    ///
    /// All three are needed before an admin can connect a GitHub installation:
    /// the slug builds the install link, and the client id/secret pair is what
    /// verifies that the admin who came back from GitHub actually administers
    /// the installation they are claiming. A deployment holding some but not
    /// all of them offers no Connect GitHub button at all — see
    /// [`Config::github_tracker_configured`], which is the one place that
    /// conjunction is computed so the console can never offer a flow the server
    /// cannot finish.
    pub github_app_slug: Option<String>,
    pub github_app_client_id: Option<String>,
    pub github_app_client_secret: Option<String>,

    /// Atlassian's OAuth client credentials, for the JIRA 3LO exchange the
    /// tracker console performs. Optional for the same reason.
    pub jira_client_id: Option<String>,
    pub jira_client_secret: Option<String>,

    /// The header a trusted proxy writes the client address into, lowercase,
    /// or `None` to use the peer address of the connection.
    ///
    /// **`None` by default, and it must stay `None` unless a proxy sits in
    /// front.** With no such proxy the header is whatever the caller typed, and
    /// an attacker rotating it walks straight through every per-IP throttle — a
    /// rate limiter keyed on a spoofable value is worse than no rate limiter,
    /// because it looks like one.
    ///
    /// **Which header is not a matter of taste.** Only a header the proxy
    /// *overwrites* is trustworthy. `X-Forwarded-For` is the conventional
    /// answer and the wrong one on any proxy that *appends* — Fly.io's does, so
    /// a caller sending `X-Forwarded-For: 1.2.3.4` arrives as
    /// `1.2.3.4, <real address>` and the left-most entry, the one every
    /// convention calls the client, is the one the attacker chose. There the
    /// right value is `fly-client-ip`, which the proxy writes itself and which
    /// carries exactly one address. Behind nginx or a load balancer configured
    /// to replace the header, `x-forwarded-for` is correct.
    pub client_ip_header: Option<String>,

    /// Whether hard-stop plans are currently being enforced against.
    ///
    /// Must match the flag `df-mcp` was built with (`DF_ENFORCE_QUOTAS`) — this
    /// never gates anything here (the console's `Meter` never calls `charge`,
    /// see `routes::usage`'s module doc), but `/api/orgs/{org}/usage` reports
    /// this value as `enforced`, and a console reporting `false` while MCP
    /// calls are actually being refused is a caller reading its own dashboard
    /// and drawing the wrong conclusion about why its agent just got a
    /// `quota_exceeded` error.
    pub enforce_quotas: bool,
}

impl Config {
    pub fn new(public_url: impl Into<String>, resource_uri: impl Into<String>) -> Self {
        Self {
            public_url: public_url.into().trim_end_matches('/').to_string(),
            resource_uri: resource_uri.into(),
            totp_issuer: "dark-factory".into(),
            github_app_webhook_secret: None,
            github_app_slug: None,
            github_app_client_id: None,
            github_app_client_secret: None,
            jira_client_id: None,
            jira_client_secret: None,
            client_ip_header: None,
            enforce_quotas: false,
        }
    }

    /// The WebAuthn relying party id: the **host** of the public URL.
    ///
    /// Derived rather than configured separately, because the two must agree —
    /// a passkey is bound to this string, and an rp_id that is not a registrable
    /// suffix of the origin makes every ceremony fail with an error that reads
    /// like a browser bug. Deriving it means one value can be wrong instead of
    /// two, and `relying_party` refuses at startup rather than at first login.
    ///
    /// **Changing the public URL's host invalidates every passkey ever
    /// registered.** Nothing here can soften that; it is what binding a
    /// credential to an origin means.
    pub fn rp_id(&self) -> Option<String> {
        self.public_url
            .split("://")
            .nth(1)?
            .split('/')
            .next()?
            .split(':')
            .next()
            .filter(|h| !h.is_empty())
            .map(str::to_string)
    }

    /// Where both providers send a browser back after authorization.
    ///
    /// One static string per deployment, registered with GitHub and Atlassian,
    /// which is exactly why it cannot be org-scoped: a redirect URI is fixed at
    /// the provider and an org slug varies per customer. The org travels in the
    /// OAuth `state` instead — see
    /// `docs/specs/2026-09-04-tracker-console-design.md` §1.
    pub fn tracker_callback_url(&self) -> String {
        self.url("/trackers/callback")
    }

    /// Whether this deployment can take an admin through connecting GitHub.
    ///
    /// The conjunction lives here rather than in the console because getting it
    /// wrong is silent in the worst direction: a Connect GitHub button on a
    /// deployment with no OAuth client sends an admin to GitHub, through an
    /// install, and back to an error — having already installed an App that now
    /// has to be uninstalled by hand.
    ///
    /// The App id and private key are deliberately *not* part of this. They are
    /// what `df-mcp` mints tokens with once a connection exists; a deployment
    /// missing them has working setup and broken sync, which is a different
    /// failure with a different message, and pretending otherwise here would
    /// hide the connection an operator needs to see to diagnose it.
    pub fn github_tracker_configured(&self) -> bool {
        self.github_app_slug.is_some()
            && self.github_app_client_id.is_some()
            && self.github_app_client_secret.is_some()
    }

    pub fn jira_tracker_configured(&self) -> bool {
        self.jira_client_id.is_some() && self.jira_client_secret.is_some()
    }

    /// The GitHub App installation link, minus its `state`.
    ///
    /// Built here rather than in the console bundle: a hard-coded App slug is
    /// how a staging console sends an admin to install the production App.
    pub fn github_install_url(&self) -> Option<String> {
        if !self.github_tracker_configured() {
            return None;
        }
        let slug = self.github_app_slug.as_ref()?;
        let mut url =
            url::Url::parse(&format!("https://github.com/apps/{slug}/installations/new")).ok()?;
        url.query_pairs_mut()
            .append_pair("redirect_uri", &self.tracker_callback_url());
        Some(url.into())
    }

    /// The Atlassian consent link, minus its `state`.
    ///
    /// `offline_access` is not optional decoration — it is what makes Atlassian
    /// return a refresh token, and without one a connection works until the
    /// first access token expires and then stops, an hour after the admin
    /// watched it succeed.
    pub fn jira_authorize_url(&self) -> Option<String> {
        if !self.jira_tracker_configured() {
            return None;
        }
        let client_id = self.jira_client_id.as_ref()?;
        let mut url = url::Url::parse("https://auth.atlassian.com/authorize").ok()?;
        url.query_pairs_mut()
            .append_pair("audience", "api.atlassian.com")
            .append_pair("client_id", client_id)
            .append_pair("scope", JIRA_SCOPES)
            .append_pair("redirect_uri", &self.tracker_callback_url())
            .append_pair("response_type", "code")
            // Atlassian returns a refresh token only when consent is actually
            // shown; without this an admin who has authorized before gets a
            // silent re-approval and no refresh token, and the connection dies
            // an hour later.
            .append_pair("prompt", "consent");
        Some(url.into())
    }

    /// Join a path onto the public URL. Every link handed to a human goes
    /// through here so there is one place a trailing slash can be got wrong.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.public_url, path.trim_start_matches('/'))
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// The WebAuthn relying party.
    ///
    /// Built once at startup from `public_url`, because its `rp_id` is what
    /// every passkey is cryptographically bound to — deriving it per request
    /// would make a configuration change silently invalidate credentials
    /// instead of failing at boot.
    pub webauthn: Arc<df_auth::passkeys::Webauthn>,
    /// Decrypts secrets at rest (currently tracker webhook secrets and JIRA
    /// OAuth credentials — see `df_core::trackers` and `df_trackers::jira`).
    /// Held as an `Arc` because the key material is
    /// loaded once at startup and shared by every request.
    pub cipher: Arc<Cipher>,
    pub config: Arc<Config>,
    /// Reads the org's plan and period counters for the usage endpoint. Never
    /// charges anything — the console is not a billable surface, and a customer
    /// looking at their own bill must not be billed for looking.
    pub meter: df_billing::Meter,
}

impl AppState {
    pub fn new(
        db: Db,
        cipher: Cipher,
        webauthn: Arc<df_auth::passkeys::Webauthn>,
        config: Config,
    ) -> Self {
        let meter = df_billing::Meter::new(
            config.enforce_quotas,
            format!("{}/settings/billing", config.public_url),
        );
        Self {
            db,
            webauthn,
            cipher: Arc::new(cipher),
            config: Arc::new(config),
            meter,
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No cipher.
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The client address, for rate limiting and the audit trail.
///
/// Returns `None` rather than a placeholder when nothing trustworthy is
/// available: `df-auth`'s throttles take an `Option`, and a made-up value like
/// `"unknown"` would put every anonymous caller in one shared bucket, where the
/// first attacker to trip it locks out everybody else.
pub fn client_ip(parts: &http::request::Parts, config: &Config) -> Option<String> {
    if let Some(header) = config.client_ip_header.as_deref() {
        if let Some(value) = parts.headers.get(header) {
            // The left-most entry is the original client; everything after it
            // was appended by intermediaries. A single-address header like
            // `Fly-Client-IP` has no comma and falls through this unchanged.
            if let Some(first) = value
                .to_str()
                .ok()
                .and_then(|v| v.split(',').next())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                return Some(first.to_string());
            }
        }
    }

    parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(headers: &[(&str, &str)]) -> http::request::Parts {
        let mut b = http::Request::builder();
        for (name, value) in headers {
            b = b.header(*name, *value);
        }
        b.body(()).unwrap().into_parts().0
    }

    fn config() -> Config {
        Config::new("https://console.test", "https://mcp.test/mcp")
    }

    #[test]
    fn no_header_is_trusted_until_one_is_named() {
        let config = config();
        assert!(
            config.client_ip_header.is_none(),
            "the default must trust nothing"
        );
        assert_eq!(
            client_ip(
                &parts(&[
                    ("x-forwarded-for", "203.0.113.9"),
                    ("fly-client-ip", "203.0.113.9"),
                ]),
                &config
            ),
            None
        );
    }

    #[test]
    fn only_the_named_header_is_read() {
        let mut config = config();
        config.client_ip_header = Some("fly-client-ip".into());

        // The header the deployment named wins, and the one it did not name is
        // not a fallback: on Fly.io `X-Forwarded-For` is caller-influenced, so
        // reading it when `Fly-Client-IP` is missing would hand an attacker the
        // value on any request they can make the proxy drop it from.
        assert_eq!(
            client_ip(
                &parts(&[
                    ("x-forwarded-for", "198.51.100.7"),
                    ("fly-client-ip", "203.0.113.9"),
                ]),
                &config
            ),
            Some("203.0.113.9".into())
        );
        assert_eq!(
            client_ip(&parts(&[("x-forwarded-for", "198.51.100.7")]), &config),
            None
        );
    }

    #[test]
    fn the_left_most_forwarded_entry_is_the_client() {
        let mut config = config();
        config.client_ip_header = Some("x-forwarded-for".into());

        assert_eq!(
            client_ip(
                &parts(&[(
                    "x-forwarded-for",
                    "203.0.113.9, 70.41.3.18, 150.172.238.178"
                )]),
                &config
            ),
            Some("203.0.113.9".into())
        );
        assert_eq!(
            client_ip(&parts(&[("x-forwarded-for", "  ")]), &config),
            None
        );
        assert_eq!(client_ip(&parts(&[]), &config), None);
    }

    #[test]
    fn urls_survive_a_trailing_slash_on_the_configured_base() {
        let config = Config::new("https://console.test/", "https://mcp.test/mcp");
        assert_eq!(config.url("/verify"), "https://console.test/verify");
        assert_eq!(config.url("verify"), "https://console.test/verify");
    }
}
