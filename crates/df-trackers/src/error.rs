use reqwest::StatusCode;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("GitHub App private key could not be parsed as a PEM-encoded RSA key")]
    InvalidGithubPrivateKey(#[source] jsonwebtoken::errors::Error),

    #[error("{provider} request failed while {action}: {source}")]
    Http {
        provider: &'static str,
        action: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("{provider} returned HTTP {status} while {action}: {body}")]
    Api {
        provider: &'static str,
        action: &'static str,
        status: StatusCode,
        body: String,
    },

    #[error("{provider} returned an invalid response while {action}: {message}")]
    InvalidResponse {
        provider: &'static str,
        action: &'static str,
        message: String,
    },

    #[error("GitHub issue state must be \"open\" or \"closed\", got {0:?}")]
    InvalidGithubIssueState(String),

    #[error("JIRA returned a refresh token that was not valid UTF-8")]
    InvalidJiraRefreshTokenEncoding,

    #[error("{0}")]
    InvalidWebhook(String),

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Core(#[from] df_core::Error),
}
