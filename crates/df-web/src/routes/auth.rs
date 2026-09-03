//! Signing up, signing in, and the second factor.
//!
//! ## There is no email
//!
//! No mailer, no verification link, no recovery link. An authenticator app is
//! the only factor; [`login_recovery_code`] is the only self-service way back in
//! without one, and an org admin resetting a member's credential is the only
//! assisted one.
//!
//! ## The bootstrap problem, and how signup resolves it
//!
//! A new account has no second factor, so it cannot log in; enrolling one needs
//! a session, so it cannot be reached. Something has to break the circle, and
//! [`signup`] is where: **it hands back a TOTP enrollment without a session, and
//! issues the session only once [`confirm_signup`] proves possession.**
//!
//! No credential exists in between. The pending secret is stored unconfirmed by
//! `totp::begin_enrollment` and is worthless to anyone who cannot produce a code
//! from it, so an abandoned signup leaves nothing to steal and no session in a
//! place nobody asked for one.
//!
//! ## What this costs, stated plainly
//!
//! [`signup`] **is** an account-enumeration oracle, and deliberately so.
//!
//! Handing the enrollment back in the HTTP response is the whole point — there
//! is no mailbox to send it to. But it must be refused for an account that
//! already has a confirmed authenticator, or typing somebody's address would
//! re-enroll their account and take it over. That refusal is a different answer
//! from the success case, and no amount of response shaping hides it: an
//! attacker who cannot tell them apart by the body can tell them apart by
//! whether a code they invent is ever accepted.
//!
//! So the constant-shape machinery that used to live here is gone rather than
//! quietly weakened, because a defense that does not hold is worse than an
//! absent one — it stops people asking the question. What *is* still defended:
//! [`login_totp`] and [`login_recovery_code`] remain indistinguishable across
//! unknown address, disabled account, wrong code and replayed code, because
//! those paths hand nothing back and have no reason to leak. Signup and login
//! now make different promises, and each says which.
//!
//! The throttle in [`throttle_by_source`] still applies, so enumeration costs a
//! request each and is rate-limited per source.

use axum::extract::{Json, State};
use axum::response::{IntoResponse, Response};
use df_auth::{login, sessions, totp};
use df_core::orgs::User;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::session::{self, CurrentUser};
use crate::state::{client_ip, AppState};

// --------------------------------------------------------------- payloads

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignupRequest {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Finish signup: the address that started it, plus a code from the app it was
/// just scanned into.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmSignupRequest {
    pub email: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub email: String,
    /// A six-digit authenticator code, or — at `/api/auth/login/recovery` — one
    /// of the codes issued at enrollment.
    pub code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmTotpRequest {
    pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpened {
    pub user: User,
    /// The account has no usable second factor and must enrol one before it can
    /// be signed into by the ordinary path again.
    pub must_enroll_totp: bool,
}

// ---------------------------------------------------------------- handlers

/// `POST /api/auth/signup` — create the account and start TOTP enrollment.
///
/// Returns the provisioning URI, the manual key, and ten recovery codes. **No
/// session and no confirmed credential exist yet**: the secret is stored
/// unconfirmed, and [`confirm_signup`] is what turns it into an account anyone
/// can sign into.
///
/// Refused for an address that already has a confirmed authenticator, because
/// handing out a fresh enrollment for one would be account takeover by typing
/// somebody's address. That refusal is the enumeration oracle the module docs
/// describe, and it is the price of having no mailbox to send a secret to.
///
/// Re-running signup for an address that started but never finished is allowed
/// and supersedes the pending secret — someone who closed the tab before
/// scanning has to be able to start again, and the abandoned secret was never
/// worth anything.
pub async fn signup(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<SignupRequest>,
) -> ApiResult<Response> {
    let email = req.email.trim().to_string();

    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::bad_request(format!(
            "{email:?} is not an email address"
        )));
    }

    throttle_by_source(&state, &parts).await?;

    // Checked before the upsert so a probe cannot create rows for addresses it
    // does not own. `upsert_user` would otherwise happily make one per guess.
    if let Some(existing) = state
        .db
        .get_user_by_email(&email)
        .await
        .map_err(ApiError::from)?
    {
        if totp::has_confirmed_credential(&state.db, existing.id).await? {
            return Err(ApiError::conflict(
                "account_exists",
                "that address already has an authenticator enrolled. Sign in with a \
                 code from it, or use a recovery code if the app is gone.",
            ));
        }
    }

    let user = state
        .db
        .upsert_user(&email, req.name.as_deref())
        .await
        .map_err(ApiError::from)?;

    let enrollment = totp::begin_enrollment(
        &state.db,
        &state.cipher,
        user.id,
        &user.email,
        &state.config.totp_issuer,
    )
    .await?;

    Ok(Json(EnrollmentResponse {
        provisioning_uri: enrollment.provisioning_uri,
        manual_key: enrollment.manual_key,
        recovery_codes: enrollment.recovery_codes,
    })
    .into_response())
}

/// `POST /api/auth/signup/confirm` — prove possession and open the first session.
///
/// The other half of [`signup`]. Takes the address rather than a session,
/// because there is no session yet — that is the whole bootstrap problem — and
/// the code itself is the proof: only someone holding the secret just issued for
/// that address can produce one.
pub async fn confirm_signup(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<ConfirmSignupRequest>,
) -> ApiResult<Response> {
    throttle_by_source(&state, &parts).await?;

    let user = state
        .db
        .get_user_by_email(req.email.trim())
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::bad_request("start by signing up with this address"))?;

    // A confirmed credential already existing means this is not a signup being
    // finished — it is someone trying to attach a second authenticator to an
    // account they may not own. Enrolling another one is a signed-in operation
    // (`POST /api/me/totp`) precisely so it cannot be reached from here.
    if totp::has_confirmed_credential(&state.db, user.id).await? {
        return Err(ApiError::conflict(
            "account_exists",
            "that address already has an authenticator enrolled. Sign in instead.",
        ));
    }

    totp::confirm_enrollment(
        &state.db,
        &state.cipher,
        user.id,
        &user.email,
        &state.config.totp_issuer,
        &req.code,
    )
    .await?;

    // No audit row here: `totp::confirm_enrollment` already writes
    // TOTP_ENROLLED, and a second one from this handler would double-count
    // every signup in the trail.
    let opened = sessions::create(&state.db, user.id).await?;

    let body = Json(SessionOpened {
        user,
        must_enroll_totp: false,
    });
    Ok(session::with_cookie(
        body.into_response(),
        session::set_cookie(&opened.token),
    ))
}

/// Limit how many signup or confirmation attempts one source may make.
///
/// Signup is the enumeration surface (see the module docs) and this is what
/// prices it: probing addresses costs a request each, per source, rather than
/// being free. It does not make the oracle go away — nothing does, once the
/// secret has to come back in the response — it makes walking a list expensive.
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

/// `POST /api/auth/login` — email plus an authenticator code.
pub async fn login_totp(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);
    let logged_in = login::with_totp(
        &state.db,
        &state.cipher,
        &req.email,
        &req.code,
        &state.config.totp_issuer,
        ip.as_deref(),
    )
    .await?;
    signed_in_response(&state, logged_in).await
}

/// `POST /api/auth/login/recovery` — email plus one of the codes from
/// enrollment.
///
/// Unlike the recovery *link*, this leaves TOTP intact: the user still holds the
/// secret, they just cannot reach it right now, and destroying it would force a
/// re-enrollment they did not ask for.
pub async fn login_recovery_code(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);
    let logged_in =
        login::with_recovery_code(&state.db, &req.email, &req.code, ip.as_deref()).await?;
    signed_in_response(&state, logged_in).await
}

async fn signed_in_response(state: &AppState, logged_in: login::LoggedIn) -> ApiResult<Response> {
    let user = state
        .db
        .get_user(logged_in.user)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;

    let body = Json(SessionOpened {
        user,
        must_enroll_totp: logged_in.must_enroll_totp,
    });

    Ok(session::with_cookie(
        body.into_response(),
        session::set_cookie(&logged_in.session_token),
    ))
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
    pub must_enroll_totp: bool,
    pub recovery_codes_remaining: i64,
}

/// `GET /api/me` — who is signed in, and what they can act in.
///
/// The console's first call on every load. Includes the org list because the
/// alternative is a second round trip before anything can be rendered.
pub async fn me(State(state): State<AppState>, caller: CurrentUser) -> ApiResult<Json<Me>> {
    let orgs = state.db.list_user_orgs(caller.user.id).await?;
    let enrolled = totp::has_confirmed_credential(&state.db, caller.user.id).await?;
    let remaining = totp::remaining_recovery_codes(&state.db, caller.user.id).await?;

    Ok(Json(Me {
        user: caller.user,
        orgs,
        must_enroll_totp: !enrolled,
        recovery_codes_remaining: remaining,
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

/// What enrollment hands back, exactly once.
///
/// Mapped field by field from `totp::Enrollment` rather than serializing that
/// type directly. `Enrollment` holds a secret and a set of live credentials, and
/// a `Serialize` derive on it would make putting them on the wire — or in a log
/// line, or a debug dump — the default rather than a deliberate act. Writing the
/// projection out here is the deliberate act.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentResponse {
    /// `otpauth://totp/…` — render as a QR code.
    pub provisioning_uri: String,
    /// The base32 secret, for manual entry when there is no camera.
    pub manual_key: String,
    /// Ten single-use codes. Shown once; only hashes are kept.
    pub recovery_codes: Vec<String>,
}

/// `POST /api/me/totp` — start enrolling an authenticator.
///
/// Returns the provisioning URI (for a QR code), the manual key, and ten
/// single-use recovery codes. **The recovery codes are shown once**, here, and
/// are stored only as hashes — a console that does not make the user save them
/// at this moment has quietly shipped an account-recovery story of "email
/// support".
pub async fn begin_totp(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<EnrollmentResponse>> {
    let enrollment = totp::begin_enrollment(
        &state.db,
        &state.cipher,
        caller.user.id,
        &caller.user.email,
        &state.config.totp_issuer,
    )
    .await?;

    Ok(Json(EnrollmentResponse {
        provisioning_uri: enrollment.provisioning_uri,
        manual_key: enrollment.manual_key,
        recovery_codes: enrollment.recovery_codes,
    }))
}

/// `POST /api/me/totp/confirm` — finish enrollment by proving possession.
pub async fn confirm_totp(
    State(state): State<AppState>,
    caller: CurrentUser,
    Json(req): Json<ConfirmTotpRequest>,
) -> ApiResult<Response> {
    totp::confirm_enrollment(
        &state.db,
        &state.cipher,
        caller.user.id,
        &caller.user.email,
        &state.config.totp_issuer,
        &req.code,
    )
    .await?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCodes {
    pub codes: Vec<String>,
}

/// `POST /api/me/recovery-codes` — issue a fresh set, invalidating the old.
///
/// For someone who has used most of theirs, or lost the piece of paper. Shown
/// once, like the set issued at enrollment.
pub async fn reissue_recovery_codes(
    State(state): State<AppState>,
    caller: CurrentUser,
) -> ApiResult<Json<RecoveryCodes>> {
    let codes = totp::issue_recovery_codes(&state.db, caller.user.id).await?;
    Ok(Json(RecoveryCodes { codes }))
}
