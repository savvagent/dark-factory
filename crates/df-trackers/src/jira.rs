use df_core::crypto::{Cipher, Sealed};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const USER_AGENT: &str = "dark-factory/0.1";
const MAX_ERROR_BODY_BYTES: usize = 256;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

impl OAuthTokens {
    pub fn seal_refresh_token(&self, cipher: &Cipher) -> Result<Sealed> {
        cipher
            .seal(self.refresh_token.as_bytes())
            .map_err(Error::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibleResource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub scopes: Vec<String>,
}

pub struct JiraClient {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    auth_base: String,
    api_base: String,
}

impl JiraClient {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            http: reqwest::Client::new(),
            auth_base: "https://auth.atlassian.com".into(),
            api_base: "https://api.atlassian.com".into(),
        }
    }

    pub async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens> {
        self.send_json(
            self.http
                .post(format!("{}/oauth/token", self.auth_base))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .json(&serde_json::json!({
                    "grant_type": "authorization_code",
                    "client_id": self.client_id,
                    "client_secret": self.client_secret,
                    "code": code,
                    "redirect_uri": redirect_uri,
                })),
            "exchanging an authorization code",
        )
        .await
    }

    pub async fn refresh_access_token(&self, refresh_token: &str) -> Result<OAuthTokens> {
        self.send_json(
            self.http
                .post(format!("{}/oauth/token", self.auth_base))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .json(&serde_json::json!({
                    "grant_type": "refresh_token",
                    "client_id": self.client_id,
                    "client_secret": self.client_secret,
                    "refresh_token": refresh_token,
                })),
            "refreshing an access token",
        )
        .await
    }

    pub async fn accessible_resources(
        &self,
        access_token: &str,
    ) -> Result<Vec<AccessibleResource>> {
        self.send_json(
            self.http
                .get(format!(
                    "{}/oauth/token/accessible-resources",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token),
            "listing accessible resources",
        )
        .await
    }

    pub async fn post_comment(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
        body: &str,
    ) -> Result<()> {
        self.send_without_response(
            self.http
                .post(format!(
                    "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}/comment",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token)
                .json(&serde_json::json!({
                    "body": {
                        "type": "doc",
                        "version": 1,
                        "content": [{
                            "type": "paragraph",
                            "content": [{
                                "type": "text",
                                "text": body,
                            }]
                        }]
                    }
                })),
            "posting an issue comment",
        )
        .await
    }

    pub async fn transition_issue(
        &self,
        access_token: &str,
        cloud_id: &str,
        issue_key: &str,
        transition_id: &str,
    ) -> Result<()> {
        self.send_without_response(
            self.http
                .post(format!(
                    "{}/ex/jira/{cloud_id}/rest/api/3/issue/{issue_key}/transitions",
                    self.api_base
                ))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .bearer_auth(access_token)
                .json(&serde_json::json!({
                    "transition": {
                        "id": transition_id,
                    }
                })),
            "transitioning an issue",
        )
        .await
    }

    pub fn open_refresh_token(cipher: &Cipher, sealed: &Sealed) -> Result<String> {
        let opened = cipher.open(&sealed.ciphertext, &sealed.nonce)?;
        String::from_utf8(opened).map_err(|_| Error::InvalidJiraRefreshTokenEncoding)
    }

    async fn send_without_response(
        &self,
        request: reqwest::RequestBuilder,
        action: &'static str,
    ) -> Result<()> {
        let response = request.send().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "JIRA",
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
            provider: "JIRA",
            action,
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            provider: "JIRA",
            action,
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Api {
                provider: "JIRA",
                action,
                status,
                body: sanitize_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            provider: "JIRA",
            action,
            message: format!("{error}; body was {}", sanitize_error_body(&body)),
        })
    }

    #[cfg(test)]
    fn with_bases(mut self, auth_base: String, api_base: String) -> Self {
        self.auth_base = auth_base;
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
                if matches!(
                    key.as_str(),
                    "token" | "access_token" | "refresh_token" | "client_secret"
                ) {
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
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;

    use super::*;
    use crate::test_support::{MockResponse, TestServer};

    #[tokio::test]
    async fn authorization_code_exchange_parses_token_response() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600,
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let tokens = client
            .exchange_code("auth-code", "https://example.com/callback")
            .await
            .expect("exchange succeeds");

        assert_eq!(
            tokens,
            OAuthTokens {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_in: 3600,
            }
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/oauth/token");
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("exchange body"),
            serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "code": "auth-code",
                "redirect_uri": "https://example.com/callback",
            })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn refresh_returns_the_rotated_pair() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 7200,
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let tokens = client
            .refresh_access_token("refresh-1")
            .await
            .expect("refresh succeeds");

        assert_eq!(tokens.access_token, "access-2");
        assert_eq!(tokens.refresh_token, "refresh-2");
        assert_eq!(tokens.expires_in, 7200);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("refresh body"),
            serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": "client-id",
                "client_secret": "client-secret",
                "refresh_token": "refresh-1",
            })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn accessible_resources_parses_the_site_list() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            serde_json::json!([
                {
                    "id": "cloud-1",
                    "name": "Engineering",
                    "url": "https://example.atlassian.net",
                    "scopes": ["read:jira-work", "write:jira-work"]
                }
            ]),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let resources = client
            .accessible_resources("access-1")
            .await
            .expect("resources succeed");

        assert_eq!(
            resources,
            vec![AccessibleResource {
                id: "cloud-1".into(),
                name: "Engineering".into(),
                url: "https://example.atlassian.net".into(),
                scopes: vec!["read:jira-work".into(), "write:jira-work".into()],
            }]
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/oauth/token/accessible-resources");
        assert_eq!(requests[0].headers["authorization"], "Bearer access-1");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn issue_calls_send_expected_method_path_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            201,
            serde_json::json!({ "id": "comment-1" }),
        ));
        server.push(MockResponse::text(204, ""));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .with_bases(server.base_url.clone(), server.base_url.clone());

        client
            .post_comment("access-1", "cloud-1", "ENG-7", "hello jira")
            .await
            .expect("comment succeeds");
        client
            .transition_issue("access-1", "cloud-1", "ENG-7", "31")
            .await
            .expect("transition succeeds");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7/comment"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).expect("comment body"),
            serde_json::json!({
                "body": {
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{
                            "type": "text",
                            "text": "hello jira"
                        }]
                    }]
                }
            })
        );
        assert_eq!(requests[1].method, "POST");
        assert_eq!(
            requests[1].path,
            "/ex/jira/cloud-1/rest/api/3/issue/ENG-7/transitions"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).expect("transition body"),
            serde_json::json!({
                "transition": { "id": "31" }
            })
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_success_jira_status_is_reported_with_status_and_body() {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            403,
            serde_json::json!({
                "errorMessages": ["forbidden"],
                "access_token": "should-not-leak"
            }),
        ));

        let client = JiraClient::new("client-id".into(), "client-secret".into())
            .with_bases(server.base_url.clone(), server.base_url.clone());
        let error = client
            .accessible_resources("access-1")
            .await
            .expect_err("resources should fail");

        match error {
            Error::Api {
                provider,
                action,
                status,
                body,
            } => {
                assert_eq!(provider, "JIRA");
                assert_eq!(action, "listing accessible resources");
                assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
                assert!(body.contains("forbidden"));
                assert!(!body.contains("should-not-leak"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        server.shutdown().await;
    }

    #[test]
    fn refresh_tokens_can_be_sealed_and_opened() {
        let cipher = Cipher::from_base64_key(&B64.encode([9u8; 32])).expect("cipher");
        let tokens = OAuthTokens {
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_in: 3600,
        };

        let sealed = tokens.seal_refresh_token(&cipher).expect("seal");
        let opened = JiraClient::open_refresh_token(&cipher, &sealed).expect("open");

        assert_eq!(opened, "refresh-1");
    }
}
