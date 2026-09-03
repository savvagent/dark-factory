-- Tracker connections (per org) and tracker bindings (per repo).
--
-- A connection says how this org reaches a provider: one GitHub App
-- installation id, one JIRA cloud site id, plus any per-org encrypted secret
-- material that provider needs. A binding says which external project or repo a
-- registered dark-factory repo maps to.

CREATE TYPE tracker_provider AS ENUM ('github', 'jira');

CREATE TABLE tracker_connections (
  id                         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                     uuid             NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  provider                   tracker_provider NOT NULL,
  -- GitHub: the App installation id (the private key that mints tokens from it
  -- is deployment config, never stored here). JIRA: the cloud site id.
  external_id                text             NOT NULL,
  -- AES-256-GCM ciphertext, canonically base64(nonce || ciphertext). NULL for
  -- GitHub: the installation id above is not secret. Holds the JIRA refresh
  -- token once that flow lands.
  encrypted_credentials      text,
  -- Reserved for a future per-connection webhook secret. NULL for both
  -- providers in Task 1.
  encrypted_webhook_secret   text,
  created_at                 timestamptz      NOT NULL DEFAULT now(),
  updated_at                 timestamptz      NOT NULL DEFAULT now(),
  UNIQUE (org_id, provider)
);

CREATE TABLE tracker_bindings (
  id                         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                     uuid             NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  repo_id                    uuid             NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  connection_id              uuid             REFERENCES tracker_connections (id) ON DELETE SET NULL,
  provider                   tracker_provider NOT NULL,
  -- GitHub: "owner/repo". JIRA: project key, e.g. "ACME".
  external_ref               text             NOT NULL,
  created_at                 timestamptz      NOT NULL DEFAULT now(),
  updated_at                 timestamptz      NOT NULL DEFAULT now(),
  UNIQUE (repo_id, provider)
);

CREATE INDEX tracker_bindings_org_id_idx ON tracker_bindings (org_id);
CREATE INDEX tracker_bindings_connection_id_idx ON tracker_bindings (connection_id);

DO $$
DECLARE
  t text;
  tenant_tables text[] := ARRAY[
    'tracker_connections',
    'tracker_bindings'
  ];
BEGIN
  FOREACH t IN ARRAY tenant_tables LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY %I ON %I USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
      t || '_tenant_isolation', t
    );
  END LOOP;
END $$;
