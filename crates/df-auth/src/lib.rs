//! `df-auth` — authentication and authorization.
//!
//! Two layers that must not be conflated:
//!
//! - **Layer 2, who the human is**: [`login`] is the front door, over [`totp`]
//!   (passwordless TOTP with replay refusal, throttling, and recovery codes)
//!   and [`sessions`] (the console's browser cookie). Enterprise OIDC
//!   federation joins this layer later.
//!
//!   **No email, anywhere.** There is no verification link, no recovery link,
//!   and no mailer — an authenticator app is the only factor, recovery codes
//!   are the only self-service way back in, and an org admin resetting a
//!   member's credential is the only assisted one. That is a product decision
//!   with a consequence worth stating plainly: signup has to hand the TOTP
//!   secret back in its own response, so it cannot also pretend not to know
//!   whether an address is already registered. See `df_web::routes::auth`.
//! - **Layer 1, what a client may do**: the OAuth 2.1 authorization server and
//!   the token model.
//!
//! We implement published standards rather than anything bespoke — OAuth 2.1
//! (authorization code + PKCE S256 only), RFC 8414 AS metadata, RFC 9728
//! protected-resource metadata, RFC 7591 dynamic client registration, RFC 8707
//! resource indicators, RFC 7009 revocation. A non-standard server would not
//! merely fail an enterprise security review; MCP clients would be unable to
//! connect to it at all.
//!
//! No password is ever accepted, stored, or reset, and no cryptographic
//! primitive is hand-written — see [`crypto`] for what is used where.

pub mod crypto;
pub mod error;
pub mod login;
pub mod oauth;
pub mod ratelimit;
pub mod sessions;
pub mod tokens;
pub mod totp;

pub use error::{AuthError, Result};
pub use login::LoggedIn;
pub use sessions::Session;
pub use tokens::Principal;
