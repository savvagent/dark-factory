-- Identity and tenancy. `orgs` is THE tenant boundary: every other tenant table
-- carries a NOT NULL org_id referencing it, and RLS (migration 0007) pins each
-- transaction to one org.

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TYPE org_role AS ENUM ('owner', 'admin', 'member');
CREATE TYPE org_plan AS ENUM ('free', 'team', 'business', 'enterprise');

-- A global human identity, keyed by verified email. One row per human no matter
-- how many orgs they belong to.
CREATE TABLE users (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  email             text        NOT NULL,
  name              text,
  email_verified_at timestamptz,
  created_at        timestamptz NOT NULL DEFAULT now(),
  disabled_at       timestamptz
);

-- Case-insensitive uniqueness without depending on the citext extension being
-- available on the host (Aurora/Neon both allow it, but this keeps the schema
-- portable to a plain Postgres).
CREATE UNIQUE INDEX users_email_key ON users (lower(email));

CREATE TABLE orgs (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  slug          text        NOT NULL,
  name          text        NOT NULL,
  plan          org_plan    NOT NULL DEFAULT 'free',
  -- Per-org job id counter. Job ids are `job-N` scoped to the org, so two orgs
  -- both have a `job-1` and neither can enumerate the other's. Bumped under the
  -- org row lock inside the insert transaction (see df-core::jobs::add_job).
  next_job_seq  bigint      NOT NULL DEFAULT 1,
  -- When true, this org's members must authenticate through its bound IdP;
  -- TOTP is refused for them.
  enforce_sso   boolean     NOT NULL DEFAULT false,
  created_at    timestamptz NOT NULL DEFAULT now(),
  deleted_at    timestamptz
);

CREATE UNIQUE INDEX orgs_slug_key ON orgs (lower(slug));

CREATE TABLE org_members (
  org_id     uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  role       org_role    NOT NULL DEFAULT 'member',
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, user_id)
);

CREATE INDEX org_members_user_idx ON org_members (user_id);

CREATE TABLE teams (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id     uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  slug       text        NOT NULL,
  name       text        NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (org_id, slug)
);

CREATE TABLE team_members (
  -- org_id is denormalized onto every tenant table so one RLS policy shape
  -- works everywhere and no policy needs a join to establish tenancy.
  org_id     uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  team_id    uuid        NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, user_id)
);

CREATE INDEX team_members_org_user_idx ON team_members (org_id, user_id);

-- Pending invitations. An invite is consumed by whoever proves control of the
-- email address, which may be a user that does not exist yet.
CREATE TABLE org_invites (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id      uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  email       text        NOT NULL,
  role        org_role    NOT NULL DEFAULT 'member',
  invited_by  uuid        REFERENCES users (id) ON DELETE SET NULL,
  token_hash  bytea       NOT NULL,
  expires_at  timestamptz NOT NULL,
  accepted_at timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX org_invites_token_key ON org_invites (token_hash);
CREATE INDEX org_invites_org_email_idx ON org_invites (org_id, lower(email));
