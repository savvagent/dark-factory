//! Orgs, members, and invitations.
//!
//! Two rules run through the whole file.
//!
//! **An org you are not in is `404`, never `403`.** Answering "forbidden" on a
//! real slug and "not found" on a fake one turns any signed-in account into a
//! directory of which companies use the product. Both collapse into one answer
//! in [`crate::session::OrgCtx`], and nothing here re-opens the distinction.
//!
//! **The last owner cannot be removed or demoted.** An org with no owner has
//! nobody who can change its billing, bind an IdP, or delete it — a state only
//! someone with database access can undo. It is refused at the last one rather
//! than repaired afterwards.

use axum::extract::{Json, Path, State};
use axum::response::{IntoResponse, Response};
use df_core::audit::{action, Entry};
use df_core::ids::UserId;
use df_core::invites::Invite;
use df_core::orgs::{Membership, Org, OrgMember, Role};
use http::request::Parts;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::mail;
use crate::session::{role_name, CurrentUser, OrgCtx};
use crate::state::{client_ip, AppState};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrgRequest {
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleRequest {
    pub role: Role,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRequest {
    pub email: String,
    #[serde(default = "default_role")]
    pub role: Role,
}

fn default_role() -> Role {
    Role::Member
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptInviteRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Joined {
    pub org: Org,
    pub role: Role,
}

/// `GET /api/orgs` — the orgs this account belongs to.
pub async fn list_orgs(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<Vec<Membership>>> {
    Ok(Json(state.db.list_user_orgs(caller.user.id).await?))
}

/// `POST /api/orgs` — create an org, with the caller as its owner.
///
/// Self-serve: any verified account may create one, which is what makes the
/// product's first five minutes work without a sales conversation. The creator
/// is the owner because an org with no owner cannot be administered at all.
pub async fn create_org(
    State(state): State<AppState>,
    caller: CurrentUser,
    Json(req): Json<CreateOrgRequest>,
) -> ApiResult<Response> {
    // An unverified address must not be able to claim an org slug — the slug is
    // a public identifier, and squatting it costs nothing if anyone can type an
    // address they do not own.
    if caller.user.email_verified_at.is_none() {
        return Err(ApiError::forbidden(
            "confirm your email address before creating an organization",
        ));
    }

    let org = state
        .db
        .create_org(req.slug.trim(), req.name.trim())
        .await?;
    state
        .db
        .add_member(org.id, caller.user.id, Role::Owner)
        .await?;

    let _ = state
        .db
        .audit_for_org(
            org.id,
            Entry::new(action::MEMBER_JOINED)
                .actor(caller.user.id)
                .target("org", org.id.to_string())
                .detail(serde_json::json!({ "role": "owner", "reason": "created the org" })),
        )
        .await;

    Ok((http::StatusCode::CREATED, Json(org)).into_response())
}

/// `GET /api/orgs/{org}` — one org, with the caller's role in it.
pub async fn get_org(ctx: OrgCtx) -> ApiResult<Json<Joined>> {
    Ok(Json(Joined {
        org: ctx.org,
        role: ctx.role,
    }))
}

/// `GET /api/orgs/{org}/members` — everyone in the org.
///
/// Readable by any member, not just admins: knowing who else is in your own org
/// is not privileged information, and a queue where you cannot tell who claimed
/// a job is not a coordination tool.
pub async fn list_members(
    State(state): State<AppState>,
    ctx: OrgCtx,
) -> ApiResult<Json<Vec<OrgMember>>> {
    Ok(Json(state.db.list_org_members(ctx.org.id).await?))
}

/// `PATCH /api/orgs/{org}/members/{user}` — change someone's role.
pub async fn set_member_role(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, user)): Path<(String, Uuid)>,
    Json(req): Json<RoleRequest>,
) -> ApiResult<Response> {
    ctx.require_admin()?;
    let target = UserId::from(user);

    let current = state
        .db
        .member_role(ctx.org.id, target)
        .await?
        .ok_or_else(|| ApiError::not_found("that user is not a member of this org"))?;

    // Only an owner may mint another owner. An admin who could promote
    // themselves to owner is an admin with owner powers one request away, which
    // makes the distinction between the two roles decorative.
    if req.role == Role::Owner || current == Role::Owner {
        ctx.require_owner()?;
    }

    if current == Role::Owner && req.role != Role::Owner {
        guard_last_owner(&state, &ctx, target).await?;
    }

    state.db.add_member(ctx.org.id, target, req.role).await?;

    let _ = state
        .db
        .audit_for_org(
            ctx.org.id,
            Entry::new(action::MEMBER_ROLE_CHANGED)
                .actor(ctx.user.id)
                .target("user", target.to_string())
                .detail(serde_json::json!({
                    "from": role_name(current),
                    "to": role_name(req.role),
                })),
        )
        .await;

    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/orgs/{org}/members/{user}` — remove someone from the org.
///
/// Also clears their team memberships and revokes the tokens they held **in
/// this org**. Leaving those behind is the whole failure: a removed member's
/// agent keeps working the queue with a token that outlives their membership by
/// up to its full lifetime, and nothing in the console would show why.
///
/// Their *sessions* are untouched, because a session is not org-scoped — it is
/// how they reach their other orgs, and signing someone out of an unrelated
/// customer's console is not this endpoint's business.
pub async fn remove_member(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, user)): Path<(String, Uuid)>,
) -> ApiResult<Response> {
    let target = UserId::from(user);

    // Leaving on your own account needs no privilege; removing someone else
    // does. Without the self case, the last member of an org they no longer
    // want cannot get out of it.
    if target != ctx.user.id {
        ctx.require_admin()?;
    }

    let current = state
        .db
        .member_role(ctx.org.id, target)
        .await?
        .ok_or_else(|| ApiError::not_found("that user is not a member of this org"))?;

    if current == Role::Owner {
        guard_last_owner(&state, &ctx, target).await?;
    }

    let mut tx = state.db.begin(ctx.org.id).await?;
    tx.remove_from_all_teams(target).await?;
    tx.audit(
        Entry::new(action::MEMBER_REMOVED)
            .actor(ctx.user.id)
            .target("user", target.to_string())
            .detail(serde_json::json!({ "role": role_name(current) })),
    )
    .await?;
    tx.commit().await?;

    state.db.remove_member(ctx.org.id, target).await?;

    let revoked = df_auth::tokens::revoke_all_in_org(&state.db, target, ctx.org.id).await?;
    tracing::info!(
        org = %ctx.org.slug,
        %target,
        revoked,
        "removed a member and revoked their tokens for this org"
    );

    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// Refuse to leave an org ownerless.
async fn guard_last_owner(state: &AppState, ctx: &OrgCtx, _target: UserId) -> ApiResult<()> {
    if state.db.count_owners(ctx.org.id).await? <= 1 {
        return Err(ApiError::conflict(
            "last_owner",
            format!(
                "{} would be left with no owner. Make someone else an owner first — \
                 an org without one has nobody who can manage billing or members.",
                ctx.org.slug
            ),
        ));
    }
    Ok(())
}

/// `POST /api/orgs/{org}/members/{user}/logout` — end every session that user
/// holds, everywhere.
///
/// The button an admin reaches for when a laptop goes missing. Deliberately
/// *not* the same action as removing them: this ends browser sessions and
/// leaves membership alone, so they can sign back in on a device that is still
/// theirs.
pub async fn force_logout(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, user)): Path<(String, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    ctx.require_admin()?;
    let target = UserId::from(user);

    state
        .db
        .member_role(ctx.org.id, target)
        .await?
        .ok_or_else(|| ApiError::not_found("that user is not a member of this org"))?;

    let revoked = df_auth::sessions::revoke_all(&state.db, target).await?;
    Ok(Json(serde_json::json!({ "sessionsRevoked": revoked })))
}

// ---------------------------------------------------------------- invites

/// `GET /api/orgs/{org}/invites` — invitations still outstanding.
pub async fn list_invites(
    State(state): State<AppState>,
    ctx: OrgCtx,
) -> ApiResult<Json<Vec<Invite>>> {
    ctx.require_admin()?;
    let mut tx = state.db.begin(ctx.org.id).await?;
    let invites = tx.list_invites().await?;
    tx.commit().await?;
    Ok(Json(invites))
}

/// `POST /api/orgs/{org}/invites` — invite someone by email.
///
/// The token is generated here and stored only as a hash, like every other
/// credential in the product; `df-core` never sees the plaintext.
///
/// **Write, commit, then send — and withdraw the invitation if the send
/// fails.** No database transaction is held open across the call to a mail
/// provider; the invitation is cleaned up afterwards instead. That ordering
/// costs a brief window in which a live invitation exists that nobody has
/// received, which is harmless — it is single-use, it expires, and a retry
/// supersedes it — and it avoids pinning a pooled connection to whatever
/// latency someone else's SMTP has today.
pub async fn create_invite(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Json(req): Json<InviteRequest>,
) -> ApiResult<Response> {
    ctx.require_admin()?;

    // Only an owner may hand out ownership.
    if req.role == Role::Owner {
        ctx.require_owner()?;
    }

    let token = df_auth::crypto::generate(df_auth::crypto::prefix::INVITE);
    let email = req.email.trim().to_string();

    // Committed before the mail goes out, and the transaction is closed first.
    // The tidy-looking alternative — hold it open and commit only after a
    // successful send — would pin a pooled connection across an unbounded call
    // to somebody else's SMTP provider, which is the same trade `watch` refuses
    // in df-billing for the same reason: a handful of slow sends would exhaust
    // the pool for every other request in the process.
    let mut tx = state.db.begin(ctx.org.id).await?;
    let invite = tx
        .create_invite(&email, req.role, Some(ctx.user.id), &token.hash)
        .await?;
    tx.audit(
        Entry::new(action::MEMBER_INVITED)
            .actor(ctx.user.id)
            .target("invite", invite.id.to_string())
            .detail(serde_json::json!({ "email": email, "role": role_name(req.role) })),
    )
    .await?;
    tx.commit().await?;

    let link = state.config.url(&format!(
        "/invite/{}?token={}",
        ctx.org.slug,
        token.into_plaintext()
    ));
    let inviter = ctx
        .user
        .name
        .clone()
        .unwrap_or_else(|| ctx.user.email.clone());

    let delivery = state
        .mailer
        .send(mail::invitation(
            &email,
            &ctx.org.name,
            &inviter,
            role_name(req.role),
            &link,
        ))
        .await;

    if let Err(e) = delivery {
        // Withdraw what nobody received. A live invitation whose link exists
        // only in a lost SMTP response is worse than no invitation: it is
        // invisible to the admin as a problem, and the "one live invite per
        // address" rule means a retry would supersede it anyway. If this
        // cleanup also fails the invite stays *visible* in the pending list,
        // where an admin can withdraw it — so the fallback degrades to
        // something a person can see and act on.
        let mut tx = state.db.begin(ctx.org.id).await?;
        if let Err(cleanup) = tx.revoke_invite(invite.id).await {
            tracing::error!(
                error = %cleanup,
                invite = %invite.id,
                "could not withdraw an invitation whose mail failed to send"
            );
        } else {
            tx.commit().await?;
        }
        return Err(e.into());
    }

    Ok((http::StatusCode::CREATED, Json(invite)).into_response())
}

/// `DELETE /api/orgs/{org}/invites/{id}` — withdraw an invitation.
pub async fn revoke_invite(
    State(state): State<AppState>,
    ctx: OrgCtx,
    Path((_org, id)): Path<(String, Uuid)>,
) -> ApiResult<Response> {
    ctx.require_admin()?;
    let mut tx = state.db.begin(ctx.org.id).await?;
    tx.revoke_invite(id).await?;
    tx.commit().await?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/orgs/{org}/invites/accept` — join, using the emailed token.
///
/// **A `POST`, like every other credential redemption here** — the emailed URL
/// points at a page that renders a button, and mail scanners that follow the
/// link find a page rather than a spent invitation.
///
/// Requires a session, and the session's *verified* address must match the one
/// invited. Signing in is what proves the address; without that check a
/// forwarded invitation mail is a way into someone else's org for whoever reads
/// it first.
pub async fn accept_invite(
    State(state): State<AppState>,
    caller: CurrentUser,
    Path(org_slug): Path<String>,
    parts: Parts,
    Json(req): Json<AcceptInviteRequest>,
) -> ApiResult<Json<Joined>> {
    if caller.user.email_verified_at.is_none() {
        return Err(ApiError::forbidden(
            "confirm your own email address before accepting an invitation",
        ));
    }

    // Not `OrgCtx`: the whole point is that the caller is not a member yet, so
    // the org is resolved by slug directly. The invitation itself is the
    // authority for admission, and it is scoped to this org by the pinned
    // transaction below.
    let org = state
        .db
        .get_org_by_slug(&org_slug)
        .await?
        .ok_or(df_core::Error::InviteInvalid)?;

    let hash = df_auth::crypto::hash(req.token.trim());

    let mut tx = state.db.begin(org.id).await?;
    let role = tx
        .accept_invite(&hash, caller.user.id, &caller.user.email)
        .await?;
    tx.audit(
        Entry::new(action::MEMBER_JOINED)
            .actor(caller.user.id)
            .from_request(client_ip(&parts, &state.config).as_deref(), None)
            .target("user", caller.user.id.to_string())
            .detail(serde_json::json!({ "role": role_name(role) })),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(Joined { org, role }))
}
