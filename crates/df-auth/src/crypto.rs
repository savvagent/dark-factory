//! Token generation, hashing, and secret encryption.
//!
//! Nothing here invents a scheme. Random bytes from the OS, SHA-256 from
//! RustCrypto, AES-256-GCM from RustCrypto, constant-time comparison from
//! `subtle`. The only decisions worth documenting are *which* primitive applies
//! where, and those are below.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{AuthError, Result};

/// Bytes of entropy in every generated credential.
///
/// 256 bits. These are bearer credentials with no rate limit on offline
/// guessing if a hash ever leaks, so there is no reason to be clever or frugal.
const TOKEN_BYTES: usize = 32;

/// Prefixes make a leaked credential identifiable on sight — in a log, a
/// bug report, or a secret scanner. Distinct per kind so an access token
/// pasted where a PAT belongs fails loudly rather than subtly.
pub mod prefix {
    pub const ACCESS: &str = "df_at_";
    pub const REFRESH: &str = "df_rt_";
    pub const AUTH_CODE: &str = "df_ac_";
    pub const SESSION: &str = "df_ss_";
    pub const PAT: &str = "df_pat_";
    pub const MAGIC: &str = "df_ml_";
    pub const INVITE: &str = "df_inv_";
    pub const RECOVERY: &str = "df_rc_";
}

/// A freshly minted credential: the plaintext to hand out **once**, and the
/// hash to store.
///
/// Deliberately not `Clone` and deliberately without a `Display`/`Debug` that
/// reveals the plaintext, so a credential cannot end up in a log line by
/// accident.
pub struct Secret {
    plaintext: String,
    pub hash: Vec<u8>,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

impl Secret {
    /// Consume the wrapper to get the plaintext. Consuming rather than
    /// borrowing forces the caller to decide, once, where it goes.
    pub fn into_plaintext(self) -> String {
        self.plaintext
    }

    pub fn expose(&self) -> &str {
        &self.plaintext
    }
}

/// Mint a new credential with the given prefix.
pub fn generate(prefix: &str) -> Secret {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    let plaintext = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(buf));
    let hash = hash(&plaintext);
    Secret { plaintext, hash }
}

/// Hash a credential for storage.
///
/// **SHA-256, not Argon2 — on purpose.** Password hashes are slow to resist
/// brute force against low-entropy human input. These are 256-bit random
/// strings: there is nothing to brute-force, and a slow hash would only add
/// latency to every authenticated request, which is a denial-of-service vector
/// rather than a defense. Argon2 belongs on passwords, and dark-factory has none.
pub fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Constant-time comparison. Used wherever a comparison result could otherwise
/// leak a secret one byte at a time through timing.
pub fn verify(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Authenticated encryption for secrets that must be *recoverable* rather than
/// merely verifiable — TOTP shared secrets, IdP client secrets, tracker refresh
/// tokens. Everything else is hashed, not encrypted.
///
/// The key comes from `DF_ENCRYPTION_KEY` in the environment (or KMS) and never
/// from the database, so a database dump alone yields no usable secret.
#[derive(Clone)]
pub struct Cipher {
    inner: Aes256Gcm,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(<key redacted>)")
    }
}

/// Ciphertext plus the nonce it was sealed under. Stored as two columns.
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl Cipher {
    /// Build from a base64 32-byte key. Accepts standard or URL-safe base64,
    /// because operators paste whatever `openssl rand -base64 32` produced.
    pub fn from_base64_key(encoded: &str) -> Result<Self> {
        let raw = B64
            .decode(encoded.trim())
            .or_else(|_| URL_SAFE_NO_PAD.decode(encoded.trim()))
            .map_err(|_| AuthError::Config("DF_ENCRYPTION_KEY is not valid base64".into()))?;

        if raw.len() != 32 {
            return Err(AuthError::Config(format!(
                "DF_ENCRYPTION_KEY must decode to 32 bytes, got {}",
                raw.len()
            )));
        }

        let key = Key::<Aes256Gcm>::from_slice(&raw);
        Ok(Self {
            inner: Aes256Gcm::new(key),
        })
    }

    /// Seal a secret. A fresh random 96-bit nonce every time — reusing a nonce
    /// under GCM is catastrophic, so it is never derived from anything and
    /// never reused across calls.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Sealed> {
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .inner
            .encrypt(nonce, plaintext)
            .map_err(|_| AuthError::Crypto("failed to seal secret".into()))?;

        Ok(Sealed {
            ciphertext,
            nonce: nonce_bytes.to_vec(),
        })
    }

    pub fn open(&self, sealed: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        if nonce.len() != 12 {
            return Err(AuthError::Crypto(
                "stored nonce has the wrong length".into(),
            ));
        }
        self.inner
            .decrypt(Nonce::from_slice(nonce), sealed)
            .map_err(|_| {
                // A failure here means the ciphertext was tampered with or the
                // key changed. Both are operational emergencies, not user errors.
                AuthError::Crypto("failed to open secret — wrong key or tampered ciphertext".into())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_prefixed_and_unique() {
        let a = generate(prefix::ACCESS);
        let b = generate(prefix::ACCESS);
        assert!(a.expose().starts_with("df_at_"));
        assert_ne!(a.expose(), b.expose());
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn hash_is_stable_and_verifies() {
        let s = generate(prefix::PAT);
        assert_eq!(hash(s.expose()), s.hash);
        assert!(verify(&hash(s.expose()), &s.hash));
        assert!(!verify(&hash("df_pat_wrong"), &s.hash));
    }

    /// The plaintext must never reach a log through `Debug`, which is how
    /// credentials most often escape.
    #[test]
    fn debug_does_not_leak_the_plaintext() {
        let s = generate(prefix::SESSION);
        let rendered = format!("{s:?}");
        assert!(!rendered.contains(s.expose()), "Debug leaked the token");
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn seal_and_open_round_trip() {
        let key = B64.encode([7u8; 32]);
        let c = Cipher::from_base64_key(&key).unwrap();
        let sealed = c.seal(b"totp-shared-secret").unwrap();
        assert_ne!(sealed.ciphertext, b"totp-shared-secret");
        assert_eq!(
            c.open(&sealed.ciphertext, &sealed.nonce).unwrap(),
            b"totp-shared-secret"
        );
    }

    /// GCM nonces must never repeat under one key.
    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let key = B64.encode([7u8; 32]);
        let c = Cipher::from_base64_key(&key).unwrap();
        let a = c.seal(b"same plaintext").unwrap();
        let b = c.seal(b"same plaintext").unwrap();
        assert_ne!(a.nonce, b.nonce, "nonce reused across seals");
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = B64.encode([7u8; 32]);
        let c = Cipher::from_base64_key(&key).unwrap();
        let mut sealed = c.seal(b"secret").unwrap();
        sealed.ciphertext[0] ^= 0xff;
        assert!(c.open(&sealed.ciphertext, &sealed.nonce).is_err());
    }

    #[test]
    fn wrong_key_cannot_open() {
        let a = Cipher::from_base64_key(&B64.encode([1u8; 32])).unwrap();
        let b = Cipher::from_base64_key(&B64.encode([2u8; 32])).unwrap();
        let sealed = a.seal(b"secret").unwrap();
        assert!(b.open(&sealed.ciphertext, &sealed.nonce).is_err());
    }

    #[test]
    fn key_must_be_32_bytes() {
        assert!(Cipher::from_base64_key(&B64.encode([0u8; 16])).is_err());
        assert!(Cipher::from_base64_key("not base64!!").is_err());
    }
}
