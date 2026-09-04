//! Which tool calls consume the bucket.
//!
//! **Not every call is billable, and this is a product decision before it is a
//! technical one.** `watch` is a thirty-second long poll that every connected
//! agent calls continuously. Billing it flat would charge an idle agent roughly
//! 86,000 operations a month for doing nothing, which makes a bill impossible
//! to predict and punishes the very behaviour the server asks for — waiting
//! instead of polling.
//!
//! The rule stated to customers is: **you pay for work performed, not for
//! looking.** Every classification below has to be defensible against that
//! sentence, so each one that is not obvious carries its reason.
//!
//! Both classes are recorded regardless. Repricing later needs history to
//! reprice against, and a call that was never written down cannot be
//! reconsidered.

/// What a tool call costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Recorded, but does not consume the plan's bucket.
    Free,
    /// Consumes one operation from the plan's bucket.
    Billable,
}

impl Class {
    pub fn is_billable(self) -> bool {
        matches!(self, Class::Billable)
    }
}

/// Reads, polls, and bookkeeping.
pub const FREE: &[&str] = &[
    // Reads. Looking is free, and an agent that checks before acting is an
    // agent doing the right thing.
    "get_job",
    "list_jobs",
    "ready",
    "blocked",
    "stats",
    "list_repos",
    "resolve_repo",
    "list_leases",
    "inbox",
    "unread_count",
    "whoami",
    "usage",
    // The long poll. The whole reason this table exists.
    "watch",
    // Renewing and releasing a lease you already paid to acquire. Charging per
    // renewal would bill an agent for holding still, and charging for release
    // would create an incentive not to release — which costs everyone else the
    // branch until the lease expires. Coordination hygiene must never have a
    // price on it.
    "renew_lease",
    "release_lease",
    // Advancing your own read cursor. Bookkeeping on a read, and billing it
    // would penalise the agents that keep their inbox tidy.
    "ack_messages",
];

/// Work.
pub const BILLABLE: &[&str] = &[
    "add_job",
    "update_job",
    "delete_job",
    "claim_jobs",
    "activate_job",
    "complete_job",
    "fail_job",
    "repend_job",
    "set_dependencies",
    "send_message",
    "acquire_lease",
    "register_repo",
    // Same category as register_repo: it changes what the org has registered.
    "update_repo",
    "link_ticket",
    "sync_ticket",
];

/// Classify a tool call.
///
/// An unrecognized tool is **free**, and loudly. The failure mode of the
/// alternative is worse in a way that is not symmetric: defaulting to billable
/// means a tool someone forgot to classify silently charges customers for
/// something nobody decided to charge for, and the first person to notice is a
/// customer reading a bill. Defaulting to free means we under-bill until
/// someone reads the log. `exhaustive_over` turns the whole question into a
/// test failure instead.
pub fn classify(tool: &str) -> Class {
    if BILLABLE.contains(&tool) {
        return Class::Billable;
    }
    if FREE.contains(&tool) {
        return Class::Free;
    }

    tracing::warn!(
        tool,
        "unclassified tool call recorded as free; add it to df_billing::classify"
    );
    Class::Free
}

/// Tools named in this table that the caller does not know about, and tools the
/// caller has that this table does not classify.
///
/// Exists to be asserted on in a test: the tool surface and the price list are
/// two lists that have to agree, maintained in different crates, and nothing
/// else notices when they stop agreeing.
pub fn exhaustive_over<'a>(tools: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();

    for tool in tools {
        seen.push(tool.to_string());
        if !BILLABLE.contains(&tool) && !FREE.contains(&tool) {
            problems.push(format!("{tool} is not classified as free or billable"));
        }
    }

    for known in FREE.iter().chain(BILLABLE.iter()) {
        if !seen.iter().any(|s| s == known) {
            problems.push(format!("{known} is classified but no such tool exists"));
        }
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_tool_is_never_in_both_columns() {
        for tool in FREE {
            assert!(
                !BILLABLE.contains(tool),
                "{tool} is classified as both free and billable"
            );
        }
    }

    #[test]
    fn no_duplicates_within_a_column() {
        for (name, list) in [("FREE", FREE), ("BILLABLE", BILLABLE)] {
            let unique: HashSet<_> = list.iter().collect();
            assert_eq!(unique.len(), list.len(), "{name} lists a tool twice");
        }
    }

    /// The one classification the pricing model depends on. If `watch` ever
    /// becomes billable, an idle agent's bill goes from nothing to tens of
    /// thousands of operations a month.
    #[test]
    fn the_long_poll_is_free() {
        assert_eq!(classify("watch"), Class::Free);
    }

    #[test]
    fn work_is_billable_and_looking_is_not() {
        for tool in [
            "add_job",
            "claim_jobs",
            "activate_job",
            "complete_job",
            "acquire_lease",
        ] {
            assert_eq!(classify(tool), Class::Billable, "{tool}");
        }
        for tool in ["get_job", "ready", "stats", "inbox", "whoami"] {
            assert_eq!(classify(tool), Class::Free, "{tool}");
        }
    }

    /// Under-billing is recoverable; charging a customer for something nobody
    /// decided to charge for is not.
    #[test]
    fn an_unknown_tool_is_free() {
        assert_eq!(classify("some_tool_added_next_year"), Class::Free);
    }

    /// Every tool that is priced and built, which is what a real surface looks
    /// like.
    fn built() -> Vec<&'static str> {
        FREE.iter().chain(BILLABLE.iter()).copied().collect()
    }

    #[test]
    fn a_surface_matching_the_price_list_reports_nothing() {
        assert!(exhaustive_over(built()).is_empty());
    }

    #[test]
    fn an_unpriced_tool_on_the_surface_is_reported() {
        let mut surface = built();
        surface.push("brand_new_tool");

        let problems = exhaustive_over(surface);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("brand_new_tool"));
        assert!(problems[0].contains("not classified"));
    }

    /// The other direction: a price for something that no longer exists is dead
    /// weight, and the day it is deleted for real nobody should have to guess
    /// whether the price list still mentions it.
    #[test]
    fn a_priced_tool_missing_from_the_surface_is_reported() {
        let surface: Vec<&str> = built().into_iter().filter(|t| *t != "watch").collect();

        let problems = exhaustive_over(surface);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("watch"));
        assert!(problems[0].contains("no such tool"));
    }
}
