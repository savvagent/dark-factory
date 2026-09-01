-- Row-level security: the second, independent tenant-isolation guard.
--
-- Guard one is the df-core API shape — every query function takes an OrgId and
-- every statement says `org_id = $1`. Guard two is this file. If someone later
-- adds a query that forgets the predicate, the database still refuses to return
-- another tenant's rows. Neither guard is allowed to be the only one.
--
-- Each tenant transaction opens with (df-core::Tx::begin):
--     SET LOCAL ROLE df_app;
--     SET LOCAL app.org_id = '<uuid>';
--
-- Both statements matter, and the ROLE one is the non-obvious half. **Superusers
-- and table owners bypass RLS**, and in practice the connecting user is very
-- often one or both: the bootstrap role in local compose, the migrating role on
-- a managed instance, the owner in every `#[sqlx::test]` throwaway database.
-- Policies written without this are decorative in exactly the environments we
-- most need them to bite — verified the hard way: before `SET LOCAL ROLE` was
-- added here, two orgs could read each other's repos with every policy in place
-- and FORCE enabled.
--
-- Dropping to a non-superuser, non-owner role for the duration of the
-- transaction makes the policies apply uniformly no matter who connected.

-- The role every tenant transaction runs as. NOLOGIN: it is never a connection
-- identity, only a `SET LOCAL ROLE` target, so it needs no password and cannot
-- be used to reach the database from outside.
--
-- Roles are cluster-scoped while migrations are database-scoped, so this is
-- idempotent: `#[sqlx::test]` runs these migrations against many throwaway
-- databases in one cluster, and a bare CREATE ROLE would fail on the second.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'df_app') THEN
    CREATE ROLE df_app NOLOGIN;
  END IF;
END $$;

-- `SET LOCAL ROLE df_app` requires the connecting role to be a member of df_app
-- (a superuser may assume any role, but the application should not connect as
-- one). Granting to CURRENT_USER covers both the migrating role in tests and a
-- single-role deployment; a deployment that connects as a separate least-
-- privilege user must also `GRANT df_app TO <that user>`.
GRANT df_app TO CURRENT_USER;

GRANT USAGE ON SCHEMA public TO df_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO df_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO df_app;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO df_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO df_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT USAGE, SELECT ON SEQUENCES TO df_app;

-- Returns the org pinned to the current transaction, or NULL when unset.
--
-- NULL is the important case: `org_id = NULL` evaluates to NULL, which is not
-- TRUE, so a transaction that forgot to pin an org sees ZERO rows rather than
-- every row. Fail-closed by construction — a missing SET LOCAL surfaces as
-- "nothing found" in a test, never as a cross-tenant leak in production.
CREATE OR REPLACE FUNCTION current_org() RETURNS uuid AS $$
  SELECT NULLIF(current_setting('app.org_id', true), '')::uuid;
$$ LANGUAGE sql STABLE;

DO $$
DECLARE
  t text;
  -- Every table whose rows belong to exactly one tenant. Auth tables
  -- (users, access_tokens, oauth_clients, magic_links, claimed_domains,
  -- idp_connections) are deliberately absent: authentication has to resolve a
  -- principal BEFORE an org is known, so pinning them to current_org() would
  -- make login impossible. They are reachable only from df-auth, which runs
  -- outside the tenant role.
  tenant_tables text[] := ARRAY[
    'teams',
    'team_members',
    'org_invites',
    'repos',
    'repo_remotes',
    'repo_leases',
    'jobs',
    'job_dependencies',
    'messages',
    'message_cursors',
    'usage_events',
    'org_period_usage',
    'subscriptions'
  ];
BEGIN
  FOREACH t IN ARRAY tenant_tables LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    -- FORCE covers the case where the application role IS the table owner
    -- (single-role deployments). Belt and braces alongside SET LOCAL ROLE.
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY %I ON %I USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
      t || '_tenant_isolation', t
    );
  END LOOP;
END $$;
