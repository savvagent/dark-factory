-- Jobs: the queue itself. Every job is anchored to a repo (repo_id NOT NULL) —
-- coordination in dark-factory is repo-scoped by construction, so there is no
-- such thing as a job floating free of the repository it is work on.

CREATE TYPE job_status AS ENUM ('pending', 'in-progress', 'completed', 'failed');
CREATE TYPE tracker AS ENUM ('jira', 'github');

CREATE TABLE jobs (
  -- `job-N`, unique per org and drawn from orgs.next_job_seq. Human-readable so
  -- an agent can say "job-42" in a message and a human can find it.
  id              text        NOT NULL,
  org_id          uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  repo_id         uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  -- Denormalized from repos.team_id at insert time so team-scoped reads never
  -- need a join, and so moving a repo between teams does not silently
  -- re-classify historical work.
  team_id         uuid        REFERENCES teams (id) ON DELETE SET NULL,

  title           text        NOT NULL,
  description     text,
  status          job_status  NOT NULL DEFAULT 'pending',

  -- Reference-only in milestone 1: a JIRA key (RELMGT-3340) or a GitHub issue
  -- (owner/repo#123). The milestone-2 sync engine hangs off these two columns.
  ticket_ref      text,
  tracker         tracker,

  -- Free-form hint for which kind of agent should take this ('claude-code',
  -- 'copilot-cli', ...). Never validated against a list: agent-agnostic means
  -- an agent we have never heard of must work on day one.
  agent_type      text,

  -- dark-factory NEVER interprets this. It is where a customer's own skills,
  -- commands, and plugins store whatever their methodology needs — the generic
  -- replacement for dark-agent's opinionated success-metrics framework.
  metadata        jsonb       NOT NULL DEFAULT '{}'::jsonb,

  created_at      timestamptz NOT NULL DEFAULT now(),
  started_at      timestamptz,
  completed_at    timestamptz,
  attempts        integer     NOT NULL DEFAULT 0,
  result          text,
  error           text,

  created_by      uuid        REFERENCES users (id) ON DELETE SET NULL,
  claimed_by      uuid        REFERENCES users (id) ON DELETE SET NULL,
  -- Which agent session holds it, for `list_jobs` readability. Same rationale as
  -- repo_leases.holder_label: one human may run several agents at once.
  claimed_by_label text,

  PRIMARY KEY (org_id, id)
);

CREATE INDEX jobs_org_status_idx ON jobs (org_id, status);
CREATE INDEX jobs_repo_status_idx ON jobs (repo_id, status);
CREATE INDEX jobs_org_ticket_idx ON jobs (org_id, ticket_ref) WHERE ticket_ref IS NOT NULL;
CREATE INDEX jobs_team_idx ON jobs (team_id) WHERE team_id IS NOT NULL;

-- Dependencies are job-to-job with a real foreign key, not dark-agent's loose
-- ticket-ref strings: a dependency that cannot be resolved to a queued job is
-- rejected at insert rather than silently blocking forever. Cycles are rejected
-- in df-core::jobs::set_dependencies via a recursive reachability check.
CREATE TABLE job_dependencies (
  org_id     uuid NOT NULL,
  job_id     text NOT NULL,
  depends_on text NOT NULL,
  PRIMARY KEY (org_id, job_id, depends_on),
  FOREIGN KEY (org_id, job_id)     REFERENCES jobs (org_id, id) ON DELETE CASCADE,
  FOREIGN KEY (org_id, depends_on) REFERENCES jobs (org_id, id) ON DELETE CASCADE,
  -- A job depending on itself is the degenerate cycle; reject it in the schema
  -- so no code path can create one.
  CHECK (job_id <> depends_on)
);

CREATE INDEX job_dependencies_depends_idx ON job_dependencies (org_id, depends_on);

-- Change notification. ONE channel for the whole server, with org_id in the
-- payload, rather than a channel per org: Postgres channel names are
-- identifiers, so per-org channels would mean a LISTEN per tenant on every
-- connection. The server LISTENs once and fans out to the right waiting `watch`
-- calls in process; a payload for an org with no waiters is dropped.
CREATE OR REPLACE FUNCTION notify_change() RETURNS trigger AS $$
DECLARE
  rec record;
BEGIN
  rec := COALESCE(NEW, OLD);
  PERFORM pg_notify(
    'df_changes',
    json_build_object(
      'kind',  TG_ARGV[0],
      'org',   rec.org_id,
      'id',    rec.id::text,
      'op',    TG_OP
    )::text
  );
  RETURN NULL;  -- AFTER trigger; return value ignored
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER jobs_notify
  AFTER INSERT OR UPDATE OR DELETE ON jobs
  FOR EACH ROW EXECUTE FUNCTION notify_change('job');

CREATE TRIGGER repo_leases_notify
  AFTER INSERT OR UPDATE OR DELETE ON repo_leases
  FOR EACH ROW EXECUTE FUNCTION notify_change('lease');
