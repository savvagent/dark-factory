//! Everything a console handler needs, and the configuration it cannot infer.

use std::sync::Arc;

use df_auth::crypto::Cipher;
use df_core::Db;

use crate::mail::Mailer;

/// Deployment-dependent settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Public base URL of the console and the authorization server — the origin
    /// a browser sees. Used to build the links that go in email, so a wrong
    /// value produces links that 404 rather than links that leak.
    pub public_url: String,

    /// Canonical URI of the MCP resource server. Tokens minted here — including
    /// personal access tokens — are audienced for exactly this, and `df-mcp`
    /// refuses anything else.
    ///
    /// Configuration rather than something derived from the request, for the
    /// same reason as in `df-mcp`: a `Host` header is attacker-controlled, and
    /// an audience derived from one is not an audience check.
    pub resource_uri: String,

    /// The label an authenticator app shows next to the code. Changing it after
    /// launch orphans every enrolled device, so it is set once.
    pub totp_issuer: String,

    /// Trust `X-Forwarded-For` for the client address used in rate limiting and
    /// the audit trail.
    ///
    /// **Off by default, and it must stay off unless a proxy that overwrites the
    /// header sits in front.** With no such proxy the header is whatever the
    /// caller typed, and an attacker rotating it walks straight through every
    /// per-IP throttle — a rate limiter keyed on a spoofable value is worse than
    /// no rate limiter, because it looks like one.
    pub trust_forwarded_for: bool,

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
            trust_forwarded_for: false,
            enforce_quotas: false,
        }
    }

    /// Join a path onto the public URL. Every emailed link goes through here so
    /// there is one place a trailing slash can be got wrong.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.public_url, path.trim_start_matches('/'))
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    /// Decrypts TOTP secrets. Held as an `Arc` because the key material is
    /// loaded once at startup and shared by every request.
    pub cipher: Arc<Cipher>,
    pub mailer: Arc<dyn Mailer>,
    pub config: Arc<Config>,
    /// Reads the org's plan and period counters for the usage endpoint. Never
    /// charges anything — the console is not a billable surface, and a customer
    /// looking at their own bill must not be billed for looking.
    pub meter: df_billing::Meter,
}

impl AppState {
    pub fn new(db: Db, cipher: Cipher, mailer: Arc<dyn Mailer>, config: Config) -> Self {
        let meter = df_billing::Meter::new(
            config.enforce_quotas,
            format!("{}/settings/billing", config.public_url),
        );
        Self {
            db,
            cipher: Arc::new(cipher),
            mailer,
            config: Arc::new(config),
            meter,
        }
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No cipher, no mailer credentials.
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
    if config.trust_forwarded_for {
        if let Some(forwarded) = parts.headers.get("x-forwarded-for") {
            // The left-most entry is the original client; everything after it
            // was appended by intermediaries.
            if let Some(first) = forwarded
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

    fn parts(header: Option<&str>) -> http::request::Parts {
        let mut b = http::Request::builder();
        if let Some(v) = header {
            b = b.header("x-forwarded-for", v);
        }
        b.body(()).unwrap().into_parts().0
    }

    #[test]
    fn a_forwarded_header_is_ignored_unless_a_proxy_is_trusted() {
        let config = Config::new("https://console.test", "https://mcp.test/mcp");
        assert!(!config.trust_forwarded_for, "the default must be off");
        assert_eq!(client_ip(&parts(Some("203.0.113.9")), &config), None);
    }

    #[test]
    fn the_left_most_forwarded_entry_is_the_client() {
        let mut config = Config::new("https://console.test", "https://mcp.test/mcp");
        config.trust_forwarded_for = true;

        assert_eq!(
            client_ip(
                &parts(Some("203.0.113.9, 70.41.3.18, 150.172.238.178")),
                &config
            ),
            Some("203.0.113.9".into())
        );
        assert_eq!(client_ip(&parts(Some("  ")), &config), None);
        assert_eq!(client_ip(&parts(None), &config), None);
    }

    #[test]
    fn urls_survive_a_trailing_slash_on_the_configured_base() {
        let config = Config::new("https://console.test/", "https://mcp.test/mcp");
        assert_eq!(config.url("/verify"), "https://console.test/verify");
        assert_eq!(config.url("verify"), "https://console.test/verify");
    }
}
