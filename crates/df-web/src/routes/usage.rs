//! The usage meter, read-only.
//!
//! The same numbers the `usage` MCP tool reports, from the same
//! `df_billing::Meter::report`, so the figure in the console and the figure an
//! agent sees cannot disagree.
//!
//! **Reading your own bill is free.** The meter held in [`AppState`] is
//! constructed with enforcement off and nothing here calls `charge`. Billing a
//! customer for looking at what they have been billed is the kind of detail
//! that costs more in trust than it could ever earn in revenue — and an org
//! that has run out must be able to see *why* it has run out.

use axum::extract::{Json, State};
use df_billing::meter::Status;
use df_core::audit::AuditEvent;
use serde::Deserialize;

use crate::error::ApiResult;
use crate::session::OrgCtx;
use crate::state::AppState;

/// `GET /api/orgs/{org}/usage` — this period's usage against the plan.
pub async fn get_usage(State(state): State<AppState>, ctx: OrgCtx) -> ApiResult<Json<Status>> {
    let mut tx = state.db.begin(ctx.org.id).await?;
    let status = state.meter.report(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(status))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    /// Restrict to one family of events — `auth.`, `oauth.`, `org.`, `repo.`.
    #[serde(default)]
    pub action_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/orgs/{org}/audit` — the org's security log.
///
/// Admin-only, unlike the rest of the console's reads. Membership changes, token
/// issuance, and failed logins are exactly the trail an attacker with a
/// low-privilege session would want to read before deciding whom to target.
pub async fn get_audit(
    State(state): State<AppState>,
    ctx: OrgCtx,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEvent>>> {
    ctx.require_admin()?;

    let mut tx = state.db.begin(ctx.org.id).await?;
    let events = tx
        .audit_trail(q.action_prefix.as_deref(), q.limit.unwrap_or(100))
        .await?;
    tx.commit().await?;
    Ok(Json(events))
}
