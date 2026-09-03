use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const GITHUB_API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "dark-factory/0.1";
const JWT_IAT_SKEW_SECONDS: i64 = 30;
const INSTALLATION_TOKEN_REFRESH_SKEW_SECONDS: i64 = 30;
const MAX_ERROR_BODY_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: i64,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

pub struct GithubAppClient {
    app_id: i64,
    key: EncodingKey,
    http: reqwest::Client,
    api_base: String,
    installation_tokens: Mutex<HashMap<i64, CachedToken>>,
}

impl GithubAppClient {
    pub fn new(app_id: i64, private_key_pem: String) -> Result<Self> {
        let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(Error::InvalidGithubPrivateKey)?;
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|source| Error::Http {
                provider: "GitHub",
                action: "building the HTTP client",
                source,
            })?;

        Ok(Self {
            app_id,
            key,
            http,
            api_base: "https://api.github.com".into(),
            installation_tokens: Mutex::new(HashMap::new()),
        })
    }

    async fn installation_token(&self, installation_id: i64) -> Result<String> {
        if let Some(token) = self.cached_installation_token(installation_id)? {
            return Ok(token);
        }

        let minted = self.exchange_installation_token(installation_id).await?;
        let token = minted.token.clone();
        self.installation_tokens
            .lock()
            .map_err(|_| {
                Error::Internal(
                    "GitHub installation-token cache was poisoned; construct a new GithubAppClient"
                        .into(),
                )
            })?
            .insert(
                installation_id,
                CachedToken {
                    token: minted.token,
                    expires_at: minted.expires_at,
                },
            );
        Ok(token)
    }

    pub async fn post_comment(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<()> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}/comments",
            self.api_base
        );
        self.send_without_response(
            self.github_request(&token, reqwest::Method::POST, &url)
                .json(&serde_json::json!({ "body": body })),
            "posting an issue comment",
        )
        .await
    }

    pub async fn list_labels(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<Vec<String>> {
        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}/labels",
            self.api_base
        );
        let labels: Vec<Label> = self
            .send_json(
                self.github_request(&token, reqwest::Method::GET, &url),
                "listing issue labels",
            )
            .await?;
        Ok(labels.into_iter().map(|label| label.name).collect())
    }

    pub async fn set_issue_state(
        &self,
        installation_id: i64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        state: &str,
    ) -> Result<()> {
        if !matches!(state, "open" | "closed") {
            return Err(Error::InvalidGithubIssueState(state.to_string()));
        }

        let token = self.installation_token(installation_id).await?;
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{issue_number}",
            self.api_base
        );
        self.send_without_response(
            self.github_request(&token, reqwest::Method::PATCH, &url)
                .json(&serde_json::json!({ "state": state })),
            "setting an issue state",
        )
        .await
    }

    fn cached_installation_token(&self, installation_id: i64) -> Result<Option<String>> {
        let now = Utc::now() + Duration::seconds(INSTALLATION_TOKEN_REFRESH_SKEW_SECONDS);
        let cached = self
            .installation_tokens
            .lock()
            .map_err(|_| {
                Error::Internal(
                    "GitHub installation-token cache was poisoned; construct a new GithubAppClient"
                        .into(),
                )
            })?
            .get(&installation_id)
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.token.clone());
        Ok(cached)
    }

    async fn exchange_installation_token(
        &self,
        installation_id: i64,
    ) -> Result<InstallationTokenResponse> {
        let app_jwt = self.mint_app_jwt()?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base
        );
        self.send_json(
            self.github_request(&app_jwt, reqwest::Method::POST, &url),
            "minting an installation access token",
        )
        .await
    }

    fn mint_app_jwt(&self) -> Result<String> {
        let now = Utc::now();
        let claims = AppClaims {
            iat: (now - Duration::seconds(JWT_IAT_SKEW_SECONDS)).timestamp(),
            exp: (now + Duration::minutes(10)).timestamp(),
            iss: self.app_id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key).map_err(|_| {
            Error::Internal(
                "GitHub App JWT signing failed; check the configured App id and private key".into(),
            )
        })
    }

    fn github_request(
        &self,
        token: &str,
        method: reqwest::Method,
        url: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
    }

    async fn send_without_response(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<()> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "GitHub",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        Ok(())
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<T> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "GitHub",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "GitHub",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            provider: "GitHub",
            action,
            message: format!("{error}; body was {}", sanitize_error_body(&body)),
        })
    }

    #[cfg(test)]
    fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base;
        self
    }
}

fn sanitize_error_body(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut value) => {
            redact_secret_fields(&mut value);
            truncate(&value.to_string(), MAX_ERROR_BODY_BYTES)
        }
        Err(_) => truncate(body, MAX_ERROR_BODY_BYTES),
    }
}

fn redact_secret_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if matches!(key.as_str(), "token" | "access_token" | "refresh_token") {
                    *value = Value::String("<redacted>".into());
                } else {
                    redact_secret_fields(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_secret_fields(item);
            }
        }
        _ => {}
    }
}

fn truncate(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }

    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &input[..end])
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use serde::Deserialize;

    use super::*;
    use crate::test_support::{MockResponse, TestServer};

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC3XlFCGcip1ief\nEPPBKThHItEglgMYCn//hcJ1kNWnClwkYmvy3pQ49Vyi+h0GZ7FQwB7K/19e28lY\n9rF/Wp0t9WA1gLh1A6MU6wUJ0fHllWfFy9+9K1CgnLZvO+x77JRODhfb7nxYUjjl\nTBYokVvw+5njbWZnxcrSR88+PprGJt7a3sxeLQ7N/TaYlyqqputMO4BtCp7dpDxg\nesElXSkCEG4DWj2IiH2LGfDxeKaxE1JetkUafCV7uMmfQ5v74sNFY5BfZNNJBfnA\nKBzK7oOF19THBxmtVWo/tYE5aBuADAUWZKOduvbA08lr3LsldJqdo5d8HJudrlte\ndl51wCcfAgMBAAECggEAUwcDXx1CpVghH56y6FsMLvWeYJVcOEYE2APOXaJjg1un\nBhiEjXdoAPRkai06+Dv6ZzhemQcRvWdiX4RwMVyrv/QTiJZMrzsi3CVgZiZoU86X\nKtIZ8FNNEjRzTKGC/kfMjR1Hg1+UcP9l4LlXbS4IRfD+qKJQFJvULuux9JqvRRnu\nq7QkKfqX8CbMAYpxSa74pBTip1V73smUqM1rP7gt0GAOd0LG19cQqPDMeWn2h14F\nDOd/48z/1gTPCCywL3AJ9Fz4rfEJi74cSqVEinJR2yduUJzXgZ/PRhKOvKicIstL\nayRSwwC2FKuM4WtOECpl8QwPn/ZV7ATxQOhcO/W55QKBgQDsRen/Ckrju3dC9zlf\nZ4+cCBFMQMjR6/gTox+4obxsZ0WynLbc5MqHP565VfUmOsl2X3aiLz/0kiiRdvQf\nLcorsA6ujYXEyRSNTAeHfzWgtaDw9dKEtLSSNjB6XJuSsYhWHvpzC2mManeWi3/0\nKFbZU+EQgDcWlLnY8W9ns6fDlQKBgQDGrZ9OU0BhIYTm8zyHrpAsrRXeOr2PkdYX\nP+ObF96UIYP8/AZBFM60/A2wvIeNei3IoVgNanLpGOcE0rHkUY5Bfd3wGAT1jXXK\nbc66rcOkI41tTebvJQClmupHSnhfLaGaqdxio7Syn5sj8qDbXsB9AaN/wAWLOd76\nhdMIZ/hS4wKBgQCsK1YT7uAbiqOhPJ2mE8TmIkrYkezEa3redGPNGq4/IBH90Yy+\n8klSvN1gmG6HaRcdFvtPu7aS9V5ygYfqoGdN5oEMWTw85XoAbIKgDeZ6MWARtk+t\nPDDIyowQ3iLPhmaeuvwtkQdctshl/0lCFZMT0reSWpvJ7J5wo55Wpud88QKBgHCv\neyKen24357xiC1vdi5J7XWLdKDTs/2PSbdLCmBCmbckoXJe/KHqIV299jtiUirE3\nqcx6KtDAug8HPbSE+U12CVIrHWz0nfGBlHZXJhbLv2RWgfvznclP8z8aIunA5N7n\nJsOfnFaPphueetPRixWbv1Mu4zYTTcAD9SzYY4UHAoGAcW+OQP6DFWhq7Yzi6Qn3\n7B1yXbvnlYP/Tj9/bXP25DxZcT8QSNfasbfq4afePduTEjXcGWvM7wTllyMif2/X\nx3+QNzDNxUxmRAIOZqSO+XBfq8Yf7unrUX92BItKKMc/8d+/7R5vmj9GdRmS746W\nWWOZZLPetCHggLA6DEPj4QM=\n-----END PRIVATE KEY-----\n";

    #[derive(Deserialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: i64,
    }

    #[test]
    fn app_jwt_carries_expected_claims() {
        let client = GithubAppClient::new(42, TEST_KEY.into()).expect("client");
        let jwt = client.mint_app_jwt().expect("jwt");
        let payload = jwt.split('.').nth(1).expect("payload");
        let decoded = URL_SAFE_NO_PAD.decode(payload).expect("decode payload");
        let claims: Claims = serde_json::from_slice(&decoded).expect("claims");

        assert_eq!(claims.iss, 42);
        assert!(claims.exp > claims.iat);
        assert!(claims.exp - claims.iat >= 600);
    }

    #[tokio::test]
    async fn installation_token_is_reused_while_valid() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "id": 1 })));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "bug" }]),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 99, "synced")
            .await
            .expect("comment succeeds");
        let labels = client
            .list_labels(17, "octo", "repo", 99)
            .await
            .expect("labels succeed");

        assert_eq!(labels, vec!["bug".to_string()]);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/app/installations/17/access_tokens");
        assert_eq!(requests[0].headers["accept"], "application/vnd.github+json");
        assert_eq!(
            requests[0].headers["x-github-api-version"],
            GITHUB_API_VERSION
        );
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer installation-token-1"
        );
        assert_eq!(
            requests[2].headers["authorization"],
            "Bearer installation-token-1"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn expired_installation_token_is_reminted() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2000-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "id": 1 })));
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-2",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "triaged" }]),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 99, "synced")
            .await
            .expect("comment succeeds");
        client
            .list_labels(17, "octo", "repo", 99)
            .await
            .expect("labels succeed");

        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].path, "/app/installations/17/access_tokens");
        assert_eq!(
            requests[1].headers["authorization"],
            "Bearer installation-token-1"
        );
        assert_eq!(requests[2].path, "/app/installations/17/access_tokens");
        assert_eq!(
            requests[3].headers["authorization"],
            "Bearer installation-token-2"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn github_calls_send_expected_method_path_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(201, serde_json::json!({ "ok": true })));
        server.push(MockResponse::json(
            200,
            serde_json::json!([{ "name": "bug" }, { "name": "help wanted" }]),
        ));
        server.push(MockResponse::json(200, serde_json::json!({ "ok": true })));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        client
            .post_comment(17, "octo", "repo", 7, "hello world")
            .await
            .expect("comment succeeds");
        let labels = client
            .list_labels(17, "octo", "repo", 7)
            .await
            .expect("labels succeed");
        client
            .set_issue_state(17, "octo", "repo", 7, "closed")
            .await
            .expect("state succeeds");

        assert_eq!(labels, vec!["bug", "help wanted"]);
        let requests = server.requests();
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/repos/octo/repo/issues/7/comments");
        assert_eq!(requests[1].headers["accept"], "application/vnd.github+json");
        assert_eq!(
            requests[1].headers["x-github-api-version"],
            GITHUB_API_VERSION
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).expect("comment body"),
            serde_json::json!({ "body": "hello world" })
        );
        assert_eq!(requests[2].method, "GET");
        assert_eq!(requests[2].path, "/repos/octo/repo/issues/7/labels");
        assert_eq!(requests[3].method, "PATCH");
        assert_eq!(requests[3].path, "/repos/octo/repo/issues/7");
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[3].body).expect("patch body"),
            serde_json::json!({ "state": "closed" })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_success_github_status_is_reported_with_status_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({
                "token": "installation-token-1",
                "expires_at": "2099-01-01T00:00:00Z"
            }),
        ));
        server.push(MockResponse::json(
            422,
            serde_json::json!({ "message": "bad issue state", "token": "should-not-leak" }),
        ));

        let client = GithubAppClient::new(42, TEST_KEY.into())
            .expect("client")
            .with_api_base(server.base_url.clone());

        let error = client
            .set_issue_state(17, "octo", "repo", 7, "closed")
            .await
            .expect_err("request should fail");

        match error {
            Error::Api {
                provider,
                action,
                status,
                body,
            } => {
                assert_eq!(provider, "GitHub");
                assert_eq!(action, "setting an issue state");
                assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
                assert!(body.contains("bad issue state"));
                assert!(!body.contains("should-not-leak"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        server.shutdown().await;
    }
}
