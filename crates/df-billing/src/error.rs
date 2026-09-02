//! Billing errors.

pub type Result<T> = std::result::Result<T, BillingError>;

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    /// The org has spent its included operations and its plan stops rather than
    /// metering overage.
    ///
    /// Written for an LLM caller that has to decide what to do next, so it says
    /// what ran out, how much was included, and where a human can fix it. An
    /// agent that reads only "quota exceeded" will retry; one that reads this
    /// will stop and say something useful to the person it is working for.
    #[error(
        "this organization has used all {included} operations included in its {plan} plan \
         this month, so {tool} was refused. Reads still work — you can still inspect the \
         queue. To queue or claim more work, someone with billing access needs to upgrade \
         at {upgrade_url}."
    )]
    QuotaExceeded {
        tool: String,
        used: i64,
        included: i64,
        plan: String,
        upgrade_url: String,
    },

    #[error(transparent)]
    Core(#[from] df_core::Error),
}

impl BillingError {
    /// Stable machine-readable code, for the same reason `df_core::Error` has
    /// one: agents branch on this, humans read the message.
    pub fn code(&self) -> &'static str {
        match self {
            BillingError::QuotaExceeded { .. } => "quota_exceeded",
            BillingError::Core(e) => e.code(),
        }
    }

    /// Retrying an exhausted quota does not help until somebody upgrades or the
    /// month rolls over, and an agent that backs off and retries is an agent
    /// burning its own time.
    pub fn retriable(&self) -> bool {
        match self {
            BillingError::QuotaExceeded { .. } => false,
            BillingError::Core(e) => e.retriable(),
        }
    }
}
