-- Reverse index for webhook bootstrap: resolve org from a provider-native id
-- before an org-scoped transaction can exist.
--
-- Deliberately outside row-level security. The webhook path has the same shape
-- as token introspection: it has to resolve *which org* owns a presented
-- identifier before any `app.org_id` can be pinned, so a tenant-scoped table
-- cannot answer it. Unlike `tracker_connections`, this table holds no secrets
-- — only provider tag, provider-native id, org id, and the owning connection id
-- — so the RLS exemption is narrow rather than broad.

CREATE TABLE tracker_connection_index (
  provider                   tracker_provider NOT NULL,
  external_id                text             NOT NULL,
  org_id                     uuid             NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  connection_id              uuid             NOT NULL REFERENCES tracker_connections (id) ON DELETE CASCADE,
  PRIMARY KEY (provider, external_id)
);

CREATE INDEX tracker_connection_index_org_id_idx ON tracker_connection_index (org_id);
CREATE INDEX tracker_connection_index_connection_id_idx ON tracker_connection_index (connection_id);
