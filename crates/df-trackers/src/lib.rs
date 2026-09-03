//! Tracker clients and sync helpers.
//!
//! `df-trackers` owns outbound tracker API calls, webhook verification/parsing,
//! and the later sync engine. This task implements the GitHub App and JIRA
//! OAuth clients only; database access stays in `df-core`.

mod error;
pub mod github;
pub mod jira;

pub use error::{Error, Result};

#[cfg(test)]
mod test_support;
