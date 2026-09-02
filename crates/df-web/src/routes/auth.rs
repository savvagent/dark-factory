//! Signing up, signing in, and the second factor.
//!
//! ## Every link is consumed on `POST`
//!
//! No route in this file spends a credential on a `GET`. The emailed URL points
//! at a console *page*; the page renders a button; the button `POST`s the token
//! to one of the endpoints here. Corporate mail scanners, link-preview
//! fetchers, and antivirus gateways follow every URL in every message they
//! handle, and a single-use `GET` is spent before the human ever clicks it. The
//! resulting failure — "your link has already been used" on the first click —
//! looks exactly like an attack and is not.
//!
//! ## The bootstrap problem, and how verification resolves it
//!
//! A new account has no second factor, so it cannot log in; enrolling one needs
//! a session, so it cannot be reached. Something has to break the circle, and
//! [`verify`] is where: **a verification link opens a session only for an
//! account that has no confirmed TOTP credential.**
//!
//! That is a rule about what the link is worth, not a convenience. Before
//! enrollment, control of the mailbox is the strongest factor the account has,
//! and it is the same factor the user would present anyway. The moment a
//! confirmed authenticator exists, the link stops being enough on its own — a
//! verification mail opened later on a phone marks the address verified and
//! nothing else. `df_auth::login::verify_email` deliberately opens no session
//! for exactly this reason, and leaves the decision here.
//!
//! ## Constant shape
//!
//! [`signup`] and [`request_link`] answer identically whether or not the
//! address is known. `df-auth` spends a whole module on making login
//! indistinguishable across unknown / disabled / wrong-code / replayed; an
//! endpoint here that said "no such user" would hand back the enumeration
//! oracle in a single line.

use axum::extract::{Json, State};
use axum::response::{IntoResponse, Response};
use df_auth::magic::Purpose;
use df_auth::{login, magic, sessions, totp};
use df_core::orgs::User;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::mail;
use crate::session::{self, CurrentUser};
use crate::state::{client_ip, AppState};

/// Floor `request_link`'s total handling time is padded up to. See that
/// handler's comment: this absorbs the latency difference between minting
/// and mailing a link (known address) and doing neither (unknown address)
/// for any mail provider fast enough to fit inside it.
const LINK_RESPONSE_FLOOR: std::time::Duration = std::time::Duration::from_millis(150);

// --------------------------------------------------------------- payloads

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignupRequest {
    pub email: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRequest {
    pub email: String,
    /// `verify` to confirm an address, `recover` to get back in without an
    /// authenticator.
    pub purpose: LinkPurpose,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPurpose {
    Verify,
    Recover,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRequest {
    pub token: String,
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

/// The answer to every "we have sent you something" request.
///
/// One shape, always. There is no field here that varies with whether the
/// address exists, because any such field is the enumeration oracle again.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Accepted {
    pub sent: bool,
    pub message: &'static str,
}

impl Accepted {
    fn new() -> Self {
        Self {
            sent: true,
            message: "If that address can receive it, a link is on its way. \
                      It works once and expires in 10 minutes.",
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpened {
    pub user: User,
    /// The account has no usable second factor and must enrol one before it can
    /// be signed into by the ordinary path again.
    pub must_enroll_totp: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verified {
    pub email_verified: bool,
    pub must_enroll_totp: bool,
    /// Whether this response also carried a session cookie. See the module
    /// docs: only an account with no confirmed authenticator gets one.
    pub signed_in: bool,
}

// ---------------------------------------------------------------- handlers

/// `POST /api/auth/signup` — create the account and mail a verification link.
pub async fn signup(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<SignupRequest>,
) -> ApiResult<Response> {
    let email = req.email.trim().to_string();

    // A malformed address is a client bug, not an enumeration probe, so it is
    // reported plainly. Everything past this point is constant-shape.
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::bad_request(format!(
            "{email:?} is not an email address"
        )));
    }

    throttle_by_source(&state, &parts).await?;

    state
        .db
        .upsert_user(&email, req.name.as_deref())
        .await
        .map_err(ApiError::from)?;

    send_link(&state, &email, Purpose::VerifyEmail).await?;
    Ok((http::StatusCode::ACCEPTED, Json(Accepted::new())).into_response())
}

/// `POST /api/auth/link` — mail a verification or recovery link.
///
/// Reachable without a session, because both callers are people who cannot get
/// one: someone whose verification mail never arrived, and someone whose phone
/// is gone.
///
/// Throttled twice, and both are needed. `magic::issue` limits per address,
/// which protects one stranger's mailbox from being flooded; that alone does
/// nothing against a script walking a list of a thousand addresses, so
/// [`throttle_by_source`] limits per source as well.
pub async fn request_link(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<LinkRequest>,
) -> ApiResult<Response> {
    let email = req.email.trim().to_string();
    let purpose = match req.purpose {
        LinkPurpose::Verify => Purpose::VerifyEmail,
        LinkPurpose::Recover => Purpose::RecoverTotp,
    };

    throttle_by_source(&state, &parts).await?;

    // Unknown addresses must get the same answer *and the same latency
    // shape* as known ones, or a timing measurement answers the question the
    // identical response body is written to refuse. Minting a link and
    // mailing it (a real provider, not `LogMailer`, has its own network
    // latency) only happens for a known address; padding this handler's
    // total time up to a fixed floor absorbs that difference for any
    // provider fast enough to fit inside it. It is not a complete fix for a
    // provider slow enough to exceed the floor on its own — a background
    // delivery queue would be — but it closes the gap for the common case
    // without deferring the "was the mailer even called" observation tests
    // rely on into a race against a spawned task.
    let started = std::time::Instant::now();

    // Note the order — the throttle inside `magic::issue` still runs for
    // known addresses, so this cannot be used to distinguish them by timing
    // a lockout either.
    if state
        .db
        .get_user_by_email(&email)
        .await
        .map_err(ApiError::from)?
        .is_some()
    {
        send_link(&state, &email, purpose).await?;
    }

    if let Some(remaining) = LINK_RESPONSE_FLOOR.checked_sub(started.elapsed()) {
        tokio::time::sleep(remaining).await;
    }

    Ok((http::StatusCode::ACCEPTED, Json(Accepted::new())).into_response())
}

/// Limit how many links one source may ask for, whatever addresses it names.
///
/// **Charged before the address is looked up, always.** Charging it only on the
/// path that actually sends would make the throttle itself an oracle: an
/// attacker probing addresses would watch their own budget move and learn which
/// ones exist. The bucket has to advance identically for every request or it
/// undoes the constant-shape response above it.
///
/// A `429` is the one non-constant answer these endpoints give, and an
/// acceptable one — it is keyed on traffic the caller generated themselves.
async fn throttle_by_source(state: &AppState, parts: &Parts) -> ApiResult<()> {
    let Some(ip) = client_ip(parts, &state.config) else {
        // Nothing trustworthy to key on. Deliberately not a shared "unknown"
        // bucket: the first attacker to trip it would lock out everyone else.
        return Ok(());
    };

    let bucket = format!("link:{ip}");
    df_auth::ratelimit::check(&state.db, &bucket).await?;
    df_auth::ratelimit::charge(&state.db, &bucket).await?;
    Ok(())
}

/// Mint a link, put it in a message, and hand it to the mailer.
async fn send_link(state: &AppState, email: &str, purpose: Purpose) -> ApiResult<()> {
    let issued = magic::issue(&state.db, email, purpose).await?;

    // Both links point at a console *page*, never at an endpoint. The page
    // renders a button that POSTs the token back — see the module docs.
    let mail = match purpose {
        Purpose::VerifyEmail => {
            let link = state.config.url(&format!("/verify?token={}", issued.token));
            mail::verify_email(email, &link)
        }
        Purpose::RecoverTotp => {
            let link = state
                .config
                .url(&format!("/recover?token={}", issued.token));
            mail::recover_account(email, &link)
        }
        // Invitations carry their own token and are mailed from the invites
        // handler, which knows the org. Reaching here with one is a wiring bug.
        Purpose::AcceptInvite => {
            return Err(ApiError::internal(
                "send_link",
                "invitations are mailed by the invites handler",
            ))
        }
    };

    state.mailer.send(mail).await?;
    Ok(())
}

/// `POST /api/auth/verify` — spend a verification link.
///
/// **Never a `GET`.** See the module docs.
pub async fn verify(
    State(state): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> ApiResult<Response> {
    let user_id = login::verify_email(&state.db, &req.token).await?;
    let enrolled = totp::has_confirmed_credential(&state.db, user_id).await?;

    let body = Verified {
        email_verified: true,
        must_enroll_totp: !enrolled,
        signed_in: !enrolled,
    };

    if enrolled {
        // The account already has a second factor, so the link is not enough to
        // be signed in by. Marking the address verified is all it does.
        return Ok(Json(body).into_response());
    }

    let opened = sessions::create(&state.db, user_id).await?;
    Ok(session::with_cookie(
        Json(body).into_response(),
        session::set_cookie(&opened.token),
    ))
}

/// `POST /api/auth/recover` — spend a recovery link.
///
/// Destroys the TOTP credential and opens a session, so the user can enrol a
/// new authenticator. That is the point of the link: it is reached for when the
/// old authenticator is gone.
pub async fn recover(
    State(state): State<AppState>,
    parts: Parts,
    Json(req): Json<TokenRequest>,
) -> ApiResult<Response> {
    let ip = client_ip(&parts, &state.config);
    let logged_in = login::recover_with_magic_link(&state.db, &req.token, ip.as_deref()).await?;
    signed_in_response(&state, logged_in).await
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
