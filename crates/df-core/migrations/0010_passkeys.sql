-- Passkeys replace TOTP.
--
-- The credential is now an origin-bound key pair held by the user's
-- authenticator, not a shared secret this database has a copy of. Three
-- consequences shape everything below:
--
-- 1. **Nothing here is a credential we could leak.** `passkeys.credential`
--    holds a public key and a signature counter. Losing this table entirely
--    would let nobody sign in as anybody.
-- 2. **The account is created from the passkey, so the address comes second.**
--    `users.email` becomes nullable: an account exists the moment a key is
--    registered, and the address is a label set afterwards. This is what lets
--    signup take no identifier at all — see 0011's note and
--    `df_web::routes::auth`.
-- 3. **There is no recovery secret.** `recovery_codes` is dropped rather than
--    carried over: a static code that bypasses a phishing-resistant credential
--    is the weakest link, and reintroducing one would undo the reason for this
--    migration. Recovery is a second passkey, or an admin-issued claim code.

DROP TABLE IF EXISTS totp_used_steps;
DROP TABLE IF EXISTS totp_credentials;
DROP TABLE IF EXISTS recovery_codes;

-- One row per registered authenticator. A user is *expected* to have several —
-- that is the recovery story, and the console pushes for a second one.
CREATE TABLE passkeys (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id       uuid        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- The raw credential ID, indexed because authentication arrives holding one
  -- and nothing else.
  credential_id bytea       NOT NULL,
  -- webauthn-rs's `Passkey`, serialised. Public key material and a counter;
  -- opaque to SQL on purpose, because its internals are that crate's business
  -- and pulling them into columns would freeze its representation here.
  credential    jsonb       NOT NULL,
  -- "MacBook", "YubiKey". Set by the user so a list of keys is a list they can
  -- act on — an unlabelled set of credential IDs is one nobody dares delete.
  nickname      text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_used_at  timestamptz
);

CREATE UNIQUE INDEX passkeys_credential_id_key ON passkeys (credential_id);
CREATE INDEX passkeys_user_idx ON passkeys (user_id, created_at DESC);

-- A WebAuthn ceremony is two round trips, and the server must remember the
-- challenge it issued between them.
--
-- **Server-side, always.** The challenge state is what binds a signature to a
-- request this server actually made; handing it to the client to give back
-- would let an attacker replay one they kept. webauthn-rs says as much in
-- capitals, and the `danger-allow-state-serialisation` feature exists so it can
-- be stored somewhere like this rather than held in process memory — which
-- would break the moment a second machine answered the second request.
CREATE TABLE webauthn_ceremonies (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  -- 'register' or 'authenticate'. Checked on redemption: a registration state
  -- must never be finishable as an authentication.
  kind       text        NOT NULL,
  -- Null for sign-in, which is deliberately usernameless — the whole point is
  -- that no identifier is submitted before the ceremony completes.
  user_id    uuid        REFERENCES users(id) ON DELETE CASCADE,
  state      jsonb       NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX webauthn_ceremonies_expiry_idx ON webauthn_ceremonies (expires_at);

-- The address is no longer how an account comes into existence, so it can be
-- absent. It stays unique when set: it is what an admin types to invite a
-- colleague and what makes an audit row legible.
--
-- The existing unique index is on `lower(email)`, and Postgres lets any number
-- of rows share a NULL there — which is exactly right. Several accounts may
-- have no address; none may share one.
ALTER TABLE users ALTER COLUMN email DROP NOT NULL;

-- Re-registration after an admin clears someone's passkeys.
--
-- Without this, an account with no passkeys is claimable by whoever reaches
-- signup first, which is the takeover this replaces: the reset was supposed to
-- grant nothing, and instead it opened a race that any stranger could win. A
-- claim is single-use, expiring, and bound to one account.
--
-- Hashed like every other credential here — the plaintext is returned once, to
-- the admin who asked for it, and never stored.
CREATE TABLE account_claims (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id     uuid        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash  bytea       NOT NULL,
  issued_by   uuid        REFERENCES users(id) ON DELETE SET NULL,
  expires_at  timestamptz NOT NULL,
  consumed_at timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX account_claims_token_key ON account_claims (token_hash);
CREATE INDEX account_claims_user_idx ON account_claims (user_id) WHERE consumed_at IS NULL;
