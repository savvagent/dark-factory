-- Authentication. Two distinct layers live here and must not be conflated:
--   Layer 2 (who the human is): totp_*, recovery_codes, magic_links,
--            idp_connections, claimed_domains, user_identities, browser_sessions.
--   Layer 1 (what a client may do): oauth_clients, authorization_codes,
--            access_tokens, refresh_tokens.
-- Nothing here stores a password, because dark-factory never accepts one.

CREATE TYPE token_kind AS ENUM ('oauth', 'pat');

-- ---------------------------------------------------------------- layer 2 ---

-- TOTP shared secrets, encrypted at rest with AES-256-GCM under DF_ENCRYPTION_KEY.
-- The key lives in the environment/KMS and never in this database, so a database
-- dump alone does not yield a single working second factor.
CREATE TABLE totp_credentials (
  user_id      uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  secret_ct    bytea       NOT NULL,
  secret_nonce bytea       NOT NULL,
  -- Set when the user proves possession by entering a code. An unconfirmed
  -- credential cannot be used to log in, so an interrupted enrollment never
  -- half-locks an account.
  confirmed_at timestamptz,
  created_at   timestamptz NOT NULL DEFAULT now()
);

-- Replay prevention. A TOTP code is valid for a 30-second step and we accept ±1
-- step of drift, which leaves a ~90-second window in which a phished code would
-- otherwise work twice. Recording the consumed step closes it.
CREATE TABLE totp_used_steps (
  user_id  uuid   NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  step     bigint NOT NULL,
  used_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, step)
);

CREATE INDEX totp_used_steps_gc_idx ON totp_used_steps (used_at);

-- Ten single-use codes issued at enrollment, hashed at rest. Without these a
-- lost phone is a lost account, which is not a shippable auth system.
CREATE TABLE recovery_codes (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  code_hash  bytea       NOT NULL,
  used_at    timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX recovery_codes_user_idx ON recovery_codes (user_id) WHERE used_at IS NULL;

CREATE TYPE magic_link_purpose AS ENUM ('verify_email', 'recover_totp', 'accept_invite');

CREATE TABLE magic_links (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  -- Email rather than user_id: a link may be issued before the user row exists
  -- (email verification at signup).
  email       text        NOT NULL,
  purpose     magic_link_purpose NOT NULL,
  token_hash  bytea       NOT NULL,
  expires_at  timestamptz NOT NULL,
  consumed_at timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX magic_links_token_key ON magic_links (token_hash);

-- Rate limiting / lockout for login attempts. Keyed by an opaque string so the
-- same table serves per-user, per-IP, and per-email buckets.
CREATE TABLE auth_attempts (
  id         bigserial PRIMARY KEY,
  bucket     text        NOT NULL,
  successful boolean     NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX auth_attempts_bucket_idx ON auth_attempts (bucket, created_at DESC);

-- Enterprise OIDC federation. One bound IdP per org (v1); SAML is out of scope.
CREATE TABLE idp_connections (
  id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id           uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  issuer           text        NOT NULL,
  client_id        text        NOT NULL,
  client_secret_ct bytea       NOT NULL,
  client_secret_nonce bytea    NOT NULL,
  -- Cached OIDC discovery document, refreshed lazily.
  discovery        jsonb       NOT NULL DEFAULT '{}'::jsonb,
  created_at       timestamptz NOT NULL DEFAULT now(),
  UNIQUE (org_id)
);

-- Email domains an org has claimed. A login for a claimed domain is routed to
-- that org's IdP instead of the TOTP prompt. Control is proved with a DNS TXT
-- record before verified_at is set — an unverified claim routes nobody, so
-- claiming `gmail.com` accomplishes nothing.
CREATE TABLE claimed_domains (
  org_id             uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  domain             text        NOT NULL,
  verification_token text        NOT NULL,
  verified_at        timestamptz,
  created_at         timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (domain)
);

CREATE INDEX claimed_domains_org_idx ON claimed_domains (org_id);

-- Pins an IdP subject to a user on first federated login.
CREATE TABLE user_identities (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id           uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  idp_connection_id uuid NOT NULL REFERENCES idp_connections (id) ON DELETE CASCADE,
  subject           text NOT NULL,
  created_at        timestamptz NOT NULL DEFAULT now(),
  UNIQUE (idp_connection_id, subject)
);

-- Console browser sessions (cookie value hashed at rest, like every other token).
CREATE TABLE browser_sessions (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  token_hash bytea       NOT NULL,
  expires_at timestamptz NOT NULL,
  revoked_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX browser_sessions_token_key ON browser_sessions (token_hash);
CREATE INDEX browser_sessions_user_idx ON browser_sessions (user_id);

-- ---------------------------------------------------------------- layer 1 ---

-- OAuth clients, most of them self-registered through RFC 7591 dynamic client
-- registration so an agent can connect without an admin creating a client by hand.
CREATE TABLE oauth_clients (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  client_id          text        NOT NULL,
  -- NULL for public clients (the normal case for a CLI agent using PKCE).
  client_secret_hash bytea,
  client_name        text,
  redirect_uris      jsonb       NOT NULL DEFAULT '[]'::jsonb,
  grant_types        jsonb       NOT NULL DEFAULT '["authorization_code","refresh_token"]'::jsonb,
  software_id        text,
  registered_via_dcr boolean     NOT NULL DEFAULT true,
  created_at         timestamptz NOT NULL DEFAULT now(),
  disabled_at        timestamptz
);

CREATE UNIQUE INDEX oauth_clients_client_id_key ON oauth_clients (client_id);

CREATE TABLE authorization_codes (
  code_hash            bytea PRIMARY KEY,
  client_id            text        NOT NULL,
  user_id              uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  -- The org is bound at authorization time. A token therefore cannot be pivoted
  -- to another org the same user happens to belong to.
  org_id               uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  redirect_uri         text        NOT NULL,
  -- PKCE is mandatory: S256 only, no `plain`, no missing challenge.
  code_challenge       text        NOT NULL,
  code_challenge_method text       NOT NULL DEFAULT 'S256',
  scopes               text[]      NOT NULL DEFAULT '{}',
  -- RFC 8707 resource indicator. The token minted from this code is audience-
  -- bound to it, and df-mcp rejects any token whose audience is not its own
  -- canonical URI. This is the confused-deputy defense.
  resource             text        NOT NULL,
  expires_at           timestamptz NOT NULL,
  consumed_at          timestamptz,
  created_at           timestamptz NOT NULL DEFAULT now()
);

-- Access tokens are opaque random strings; only the SHA-256 hash is stored, so a
-- database read does not yield a usable credential. PATs share this table
-- because they carry identical claims — the only differences are `kind`, a
-- human-facing `name`, and a longer lifetime.
CREATE TABLE access_tokens (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  token_hash   bytea       NOT NULL,
  kind         token_kind  NOT NULL DEFAULT 'oauth',
  name         text,
  user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  org_id       uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  client_id    text,
  scopes       text[]      NOT NULL DEFAULT '{}',
  resource     text        NOT NULL,
  expires_at   timestamptz NOT NULL,
  revoked_at   timestamptz,
  last_used_at timestamptz,
  created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX access_tokens_token_key ON access_tokens (token_hash);
CREATE INDEX access_tokens_org_user_idx ON access_tokens (org_id, user_id);

-- Refresh tokens rotate: redeeming one consumes it and issues a successor. A
-- replayed (already-consumed) refresh token is treated as theft and revokes the
-- whole chain — see df-auth::oauth::refresh.
CREATE TABLE refresh_tokens (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  token_hash      bytea       NOT NULL,
  access_token_id uuid        REFERENCES access_tokens (id) ON DELETE SET NULL,
  user_id         uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  org_id          uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  client_id       text        NOT NULL,
  scopes          text[]      NOT NULL DEFAULT '{}',
  resource        text        NOT NULL,
  expires_at      timestamptz NOT NULL,
  consumed_at     timestamptz,
  rotated_to      uuid        REFERENCES refresh_tokens (id) ON DELETE SET NULL,
  revoked_at      timestamptz,
  created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX refresh_tokens_token_key ON refresh_tokens (token_hash);
CREATE INDEX refresh_tokens_user_idx ON refresh_tokens (user_id, org_id);
