-- Audit trail.
--
-- Enterprise security review always asks who did what, from where, and when —
-- and that is history you cannot backfill. `auth_attempts` (0005) does not serve
-- this: it is an ephemeral rate-limiting bucket keyed by an opaque string, not
-- an attributable record. So this table exists from the first auth path rather
-- than being retrofitted when the first questionnaire arrives.

CREATE TABLE audit_events (
  id            bigserial PRIMARY KEY,

  -- Nullable on purpose. Most events belong to an org and are readable by that
  -- org's admins under the RLS policy below. Some genuinely precede any org
  -- context — a login attempt, a TOTP enrollment, an email verification — and
  -- those are written with a NULL org.
  --
  -- The consequence is deliberate and fail-closed: `org_id = current_org()` is
  -- NULL for a NULL row, which is not TRUE, so org-scoped rows are the only
  -- ones a tenant can ever see. NULL-org rows are reachable only from the
  -- unpinned control plane (df-auth), which is exactly the intent.
  org_id        uuid        REFERENCES orgs (id) ON DELETE CASCADE,

  actor_user_id uuid        REFERENCES users (id) ON DELETE SET NULL,
  -- Who acted when it was not a browser session: a PAT name, an OAuth
  -- client_id, or 'system' for background work. The user id above still
  -- attributes it to a human where one exists.
  actor_label   text,

  -- Dotted, stable, and enumerated in df_core::audit::action. Queried by
  -- prefix ('auth.%'), so the namespace ordering matters.
  action        text        NOT NULL,
  target_type   text,
  target_id     text,

  -- Stored as text, not inet: we never do subnet arithmetic on it, and text
  -- avoids a type dependency for zero benefit. Carries IPv4, IPv6, or a
  -- forwarded-for chain equally well.
  ip            text,
  user_agent    text,

  -- Event-specific context. Never secrets: this table is readable by org
  -- admins in the console.
  detail        jsonb       NOT NULL DEFAULT '{}'::jsonb,

  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_org_time_idx ON audit_events (org_id, created_at DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, created_at DESC);
CREATE INDEX audit_events_action_idx ON audit_events (action, created_at DESC);

ALTER TABLE audit_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_events FORCE ROW LEVEL SECURITY;

-- Read policy, matching every other tenant table.
CREATE POLICY audit_events_tenant_isolation ON audit_events
  USING (org_id = current_org())
  WITH CHECK (org_id = current_org());

-- **Append-only.** An audit trail an attacker can edit is not an audit trail.
-- The tenant role may write and read; it may not rewrite or erase. Deleting old
-- events is a retention job run by the control plane, not something reachable
-- from a request.
REVOKE UPDATE, DELETE ON audit_events FROM df_app;
GRANT SELECT, INSERT ON audit_events TO df_app;
GRANT USAGE, SELECT ON SEQUENCE audit_events_id_seq TO df_app;
