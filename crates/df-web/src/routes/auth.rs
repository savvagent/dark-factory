//! Signing up, signing in, and the passkey that is both.
//!
//! ## No email, and now no identifier either
//!
//! A passkey brings an account into existence: [`signup_start`] creates the row
//! with **no address at all**, and [`signup_finish`] turns it into an account
//! once a credential is registered against it. The address is a profile field
//! set afterwards, by someone already holding the key.
//!
//! That ordering is what finally closes the account-enumeration oracle. Removing
//! email forced signup to hand a TOTP secret back in its own response, which
//! meant refusing addresses that already had one — and that refusal was the
//! leak. Nothing is submitted to [`signup_start`] or [`login_start`], so there
//! is no question for either to answer differently. The one place the product
//! will say "that address is taken" is [`set_profile`], which needs a session.
//!
//! ## Every ceremony is two requests
//!
//! `…/start` issues a challenge and stores its state server-side; `…/finish`
//! presents the signature. The ceremony id in between is a lookup key, not a
//! credential: it is single-use, expiring, and useless without a signature the
//! matching authenticator can produce.
//!
//! ## No credential is ever spent on a `GET`
//!
//! Unchanged, and it still matters: an invitation code travels through chat,
//! and chat unfurls links.

use axum::extract::{Json, State};
use axum::response::{IntoResponse, Response};
use df_auth::{login, passkeys, sessions};
use df_core::orgs::User;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::session::{self, CurrentUser};
use crate::state::{client_ip, AppState};

// --------------------------------------------------------------- payloads

/// Finish a registration ceremony.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishRegistration {
    pub ceremony_id: uuid::Uuid,
    pub credential: passkeys::RegisterPublicKeyCredential,
    /// What to call this authenticator in the list. Worth asking for: an
    /// unlabelled set of keys is a set nobody dares delete from.
    #[serde(default)]
    pub nickname: Option<String>,
}

/// Finish an authentication ceremony.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishAuthentication {
    pub ceremony_id: uuid::Uuid,
    pub credential: passkeys::PublicKeyCredential,
}

/// Start re-registering with an admin-issued claim code.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    pub code: String,
}

/// Finish re-registering. Carries the code again, because it is spent at
/// `finish` rather than `start` — an interrupted ceremony must not burn
/// somebody's only way back into their account.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishClaim {
    pub ceremony_id: uuid::Uuid,
    pub code: String,
    pub credential: passkeys::RegisterPublicKeyCredential,
    #[serde(default)]
    pub nickname: Option<String>,
}

/// Set the profile on an account that has a passkey.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRequest {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Name a registered key.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameKeyRequest {
    pub nickname: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpened {
    pub user: User,
    /// The account holds exactly one passkey.
    ///
    /// Not a failure — it signs in fine. It is the console's cue to ask for a
    /// second one, because one passkey is one device and there is no email to
    /// recover through if it is lost.
    pub should_add_passkey: bool,
}

// ---------------------------------------------------------------- handlers

/// The challenge half of a ceremony, as the browser needs it.
///
/// `ceremonyId` is opaque and worthless on its own: the server keeps the state
/// it names, single-use and expiring, and nothing here can be redeemed without
/// a signature from an authenticator that holds the matching key.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeResponse<T> {
    pub ceremony_id: uuid::Uuid,
    pub challenge: T,
}

/// `POST /api/auth/signup/start` — create an account and challenge for a passkey.
///
/// Takes **no body**. The account is created here with no address, because the
/// passkey is what makes it an account; nothing about it is reachable by anyone
/// who does not go on to hold the key, so an abandoned signup leaves an inert
/// row rather than a claimable identity.
pub async fn signup_start(
    State(state): State<AppState>,
    parts: Parts,
) -> ApiResult<Json<ChallengeResponse<passkeys::CreationChallengeResponse>>> {
    throttle_by_source(&state, &parts).await?;

    let ceremony = passkeys::start_registration(&state.db, &state.webauthn, None).await?;
    Ok(Json(ChallengeResponse {
        ceremony_id: ceremony.id,
        challenge: ceremony.challenge,
    }))
}

/// `POST /api/auth/signup/finish` — register the passkey and open the first session.
pub async fn signup_finish(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<FinishRegistration>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);
    let user = passkeys::finish_registration(
        &state.db,
        &state.webauthn,
        req.ceremony_id,
        &req.credential,
        req.nickname.as_deref(),
    )
    .await?;

    let opened = login::with_passkey(&state.db, user, ip.as_deref()).await?;
    signed_in_response(&state, opened).await
}

/// `POST /api/auth/login/start` — challenge for any passkey this server knows.
///
/// Deliberately takes no identifier and no body. The credential the browser
/// picks is what says who is signing in.
pub async fn login_start(
    State(state): State<AppState>,
) -> ApiResult<Json<ChallengeResponse<passkeys::RequestChallengeResponse>>> {
    let ceremony = passkeys::start_authentication(&state.db, &state.webauthn).await?;
    Ok(Json(ChallengeResponse {
        ceremony_id: ceremony.id,
        challenge: ceremony.challenge,
    }))
}

/// `POST /api/auth/login/finish` — present the signature and open a session.
pub async fn login_finish(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<FinishAuthentication>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);
    let user = passkeys::finish_authentication(
        &state.db,
        &state.webauthn,
        req.ceremony_id,
        &req.credential,
        ip.as_deref(),
    )
    .await?;

    let opened = login::with_passkey(&state.db, user, ip.as_deref()).await?;
    signed_in_response(&state, opened).await
}

/// `POST /api/auth/claim/start` — begin re-registering after an admin reset.
///
/// The code is proof that an admin of an org this account belongs to issued it.
/// Without this endpoint an account with no passkeys would be claimable by
/// whoever reached signup first, which is the takeover this exists to prevent.
pub async fn claim_start(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<ClaimRequest>,
) -> ApiResult<Json<ChallengeResponse<passkeys::CreationChallengeResponse>>> {
    throttle_by_source(&state, &parts).await?;

    let user = state.db.peek_account_claim(&hash_claim(&req.code)).await?;
    let ceremony = passkeys::start_registration(&state.db, &state.webauthn, Some(user)).await?;

    Ok(Json(ChallengeResponse {
        ceremony_id: ceremony.id,
        challenge: ceremony.challenge,
    }))
}

/// `POST /api/auth/claim/finish` — register the new passkey and sign in.
///
/// The code is spent here rather than at `start`, so an interrupted ceremony
/// does not burn somebody's only way back into their account.
pub async fn claim_finish(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<FinishClaim>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);

    let user = state
        .db
        .consume_account_claim(&hash_claim(&req.code))
        .await?;
    let registered = passkeys::finish_registration(
        &state.db,
        &state.webauthn,
        req.ceremony_id,
        &req.credential,
        req.nickname.as_deref(),
    )
    .await?;

    // The ceremony was started against the claimed account; if these disagree,
    // something has been substituted and the safe answer is to refuse.
    if registered != user {
        return Err(ApiError::forbidden(
            "that claim code is not for this ceremony",
        ));
    }

    let opened = login::with_passkey(&state.db, user, ip.as_deref()).await?;
    signed_in_response(&state, opened).await
}

/// Limit how many account-creating or claim attempts one source may make.
///
/// Signup no longer leaks anything, so this is not an anti-enumeration measure
/// any more — it is what stops a script minting accounts, and what prices
/// guessing at claim codes.
async fn throttle_by_source(state: &AppState, parts: &Parts) -> ApiResult<()> {
    let Some(ip) = client_ip(parts, &state.config) else {
        // Nothing trustworthy to key on. Deliberately not a shared "unknown"
        // bucket: the first attacker to trip it would lock out everyone else.
        return Ok(());
    };

    let bucket = format!("signup:{ip}");
    df_auth::ratelimit::check(&state.db, &bucket).await?;
    df_auth::ratelimit::charge(&state.db, &bucket).await?;
    Ok(())
}

/// Attach the session cookie and describe the account that just signed in.
async fn signed_in_response(state: &AppState, logged_in: login::LoggedIn) -> ApiResult<Response> {
    let user = state
        .db
        .get_user(logged_in.user)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;

    let body = Json(SessionOpened {
        user,
        should_add_passkey: logged_in.should_add_passkey,
    });

    Ok(session::with_cookie(
        body.into_response(),
        session::set_cookie(&logged_in.session_token),
    ))
}

fn hash_claim(code: &str) -> Vec<u8> {
    df_auth::crypto::hash(code.trim())
}

/// `POST /api/auth/logout` — end this session.
///
/// Succeeds for a caller holding a cookie that resolves to nothing. Logging out
/// is not a privileged operation, and a visitor with a stale cookie asking to
/// be rid of it should be obliged.
pub async fn logout(State(state): State<AppState>, parts: Parts) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);

    if let Some(token) = session::token_from(&parts) {
        login::logout(&state.db, &token, ip.as_deref()).await?;
    }

    Ok(session::with_cookie(
        http::StatusCode::NO_CONTENT.into_response(),
        session::clear_cookie(),
    ))
}

// ------------------------------------------------------------------- me

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub user: User,
    pub orgs: Vec<df_core::orgs::Membership>,
    /// One passkey. The console nags for a second; see [`SessionOpened`].
    pub should_add_passkey: bool,
    pub passkey_count: i64,
}

/// `GET /api/me` — who is signed in, and what they can act in.
///
/// The console's first call on every load. Includes the org list because the
/// alternative is a second round trip before anything can be rendered.
pub async fn me(State(state): State<AppState>, caller: CurrentUser) -> ApiResult<Json<Me>> {
    let orgs = state.db.list_user_orgs(caller.user.id).await?;
    let passkey_count = passkeys::count(&state.db, caller.user.id).await?;

    Ok(Json(Me {
        user: caller.user,
        orgs,
        should_add_passkey: passkey_count < 2,
        passkey_count,
    }))
}

/// `GET /api/me/sessions` — where this account is signed in.
pub async fn list_sessions(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<Vec<sessions::Session>>> {
    Ok(Json(sessions::list(&state.db, caller.user.id).await?))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokedSessions {
    pub revoked: u64,
}

/// `DELETE /api/me/sessions` — sign out everywhere, including here.
///
/// Sessions only. Access tokens and PATs are a separate credential with their
/// own revocation path, and someone dealing with a lost laptop needs both — but
/// quietly killing an org's agents from a button labelled "sign out everywhere"
/// is a different decision than the button describes.
pub async fn revoke_all_sessions(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Response> {
    let revoked = sessions::revoke_all(&state.db, caller.user.id).await?;
    Ok(session::with_cookie(
        Json(RevokedSessions { revoked }).into_response(),
        session::clear_cookie(),
    ))
}

// ------------------------------------------------------- passkeys and profile

/// `POST /api/me/passkeys/start` — challenge to add another authenticator.
///
/// The recovery story, and the console asks for it straight after signup: one
/// passkey is one device, and there is no email to recover through.
pub async fn add_passkey_start(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<ChallengeResponse<passkeys::CreationChallengeResponse>>> {
    let ceremony =
        passkeys::start_registration(&state.db, &state.webauthn, Some(caller.user.id)).await?;
    Ok(Json(ChallengeResponse {
        ceremony_id: ceremony.id,
        challenge: ceremony.challenge,
    }))
}

/// `POST /api/me/passkeys/finish` — register it.
pub async fn add_passkey_finish(
    State(state): State<AppState>,
    caller: CurrentUser,
    Json(req): Json<FinishRegistration>,
) -> ApiResult<Response> {
    let registered = passkeys::finish_registration(
        &state.db,
        &state.webauthn,
        req.ceremony_id,
        &req.credential,
        req.nickname.as_deref(),
    )
    .await?;

    // The ceremony was opened for this session's account. A mismatch means one
    // was substituted, and the safe answer is to refuse rather than to attach
    // somebody's key to somebody else's account.
    if registered != caller.user.id {
        return Err(ApiError::forbidden(
            "that ceremony belongs to another account",
        ));
    }

    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/me/passkeys` — the authenticators on this account.
pub async fn list_passkeys(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<Vec<passkeys::RegisteredKey>>> {
    Ok(Json(passkeys::list(&state.db, caller.user.id).await?))
}

/// `DELETE /api/me/passkeys/{id}` — remove one, never the last.
pub async fn remove_passkey(
    State(state): State<AppState>,
    caller: CurrentUser,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> ApiResult<Response> {
    passkeys::remove(&state.db, caller.user.id, id).await?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `PATCH /api/me/passkeys/{id}` — name one.
pub async fn rename_passkey(
    State(state): State<AppState>,
    caller: CurrentUser,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
    Json(req): Json<RenameKeyRequest>,
) -> ApiResult<Response> {
    passkeys::rename(&state.db, caller.user.id, id, &req.nickname).await?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// `PATCH /api/me` — set the address and display name.
///
/// **The one place this product says "that address is taken."** It needs a
/// session, which makes the answer attributable, rate-limited and auditable —
/// unlike a signup endpoint, which a stranger can walk a list against. That is
/// the whole reason the address is set here rather than at signup.
pub async fn set_profile(
    State(state): State<AppState>,
    caller: CurrentUser,
    Json(req): Json<ProfileRequest>,
) -> ApiResult<Json<User>> {
    let updated = state
        .db
        .set_profile(
            caller.user.id,
            req.email.as_deref().filter(|e| !e.trim().is_empty()),
            req.name.as_deref().filter(|n| !n.trim().is_empty()),
        )
        .await?;
    Ok(Json(updated))
}
