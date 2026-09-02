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
            client_ip_header: None,
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
