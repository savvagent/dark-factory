//! `df-billing` — metering, buckets, and the free/billable split.
//!
//! The billable unit is the MCP tool call, but **not every call is billable**,
//! and that classification ([`classify`]) is load-bearing rather than a detail:
//! `watch` is a continuous long poll, and billing it flat would charge an idle
//! agent tens of thousands of operations a month for waiting quietly, which is
//! precisely the behaviour the server asks of it.
//!
//! The split of responsibility with `df-core` is deliberate. Every statement
//! against a tenant table lives in `df-core` and goes through a pinned `Tx`
//! (see `df_core::usage`); what lives here is policy — which tools cost
//! anything, what a bucket is worth, when a call is refused. That leaves the
//! interesting decisions unit-testable with no database at all, which is why
//! the threshold arithmetic in [`meter`] and the price list in [`classify`]
//! have tests that run in microseconds.
//!
//! The one thing that must not be refactored apart: the counter is incremented
//! **in the same transaction as the tool's own work**. A failed call is not
//! billed and a successful one is never billed twice, and both of those follow
//! from that single fact rather than from any retry logic.

pub mod classify;
pub mod error;
pub mod meter;

pub use classify::{classify, Class};
pub use error::{BillingError, Result};
pub use meter::{Charge, Meter, Status};
