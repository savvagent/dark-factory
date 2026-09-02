//! The console's handlers, grouped by what they act on.
//!
//! Every handler here is named once in [`crate::catalog`], which is what mounts
//! it and what documents it. A handler that is not in the catalog is not
//! reachable — deliberately, so that adding a route and describing it are the
//! same act.

pub mod auth;
pub mod jobs;
pub mod orgs;
pub mod repos;
pub mod teams;
pub mod tokens;
pub mod usage;
