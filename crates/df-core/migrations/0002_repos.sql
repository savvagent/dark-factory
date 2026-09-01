-- Repos: the coordination anchor. An agent's working directory becomes meaningful
-- to the server by resolving its git remote to one of these rows.

CREATE TYPE repo_provider AS ENUM ('github', 'gitlab', 'bitbucket', 'other');

CREATE TABLE repos (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id             uuid          NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  slug               text          NOT NULL,
  name               text          NOT NULL,
  provider           repo_provider NOT NULL DEFAULT 'other',
  default_branch     text          NOT NULL DEFAULT 'main',
  -- Optional owning team. When set, only that team's members and org admins see
  -- the repo and its jobs; when null the repo is org-wide.
  team_id            uuid          REFERENCES teams (id) ON DELETE SET NULL,
  -- Free-form hint only ('claude-code', 'copilot-cli', ...). Never enforced:
  -- dark-factory is agent-agnostic and must not privilege any client.
  default_agent_type text,
  -- Tracker binding, e.g. {"jiraProjects":["RELMGT"],"githubRepo":"acme/api"}.
  -- Shaped, not enforced, until the milestone-2 sync engine reads it.
  tracker_binding    jsonb         NOT NULL DEFAULT '{}'::jsonb,
  active             boolean       NOT NULL DEFAULT true,
  created_at         timestamptz   NOT NULL DEFAULT now(),
  created_by         uuid          REFERENCES users (id) ON DELETE SET NULL,
  UNIQUE (org_id, slug)
);

CREATE INDEX repos_org_active_idx ON repos (org_id, active);
CREATE INDEX repos_team_idx ON repos (team_id) WHERE team_id IS NOT NULL;

-- Every remote form that identifies a repo, NORMALIZED (see
-- df-core::repos::normalize_remote): scheme, credentials, port, trailing `.git`
-- and SSH-vs-HTTPS differences stripped, host lowercased. This is the lookup
-- index for `resolve_repo` — an agent passes whatever
-- `git remote get-url origin` gave it and lands on exactly one row.
--
-- Unique per ORG, not globally: two orgs may legitimately both work in the same
-- public repo, and neither may learn about the other by registering it.
CREATE TABLE repo_remotes (
  org_id     uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  repo_id    uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  normalized text        NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, normalized)
);

CREATE INDEX repo_remotes_repo_idx ON repo_remotes (repo_id);

-- Advisory, time-bounded leases on (repo, branch). These are how two agents on a
-- team avoid colliding in the same working tree. The server cannot see git
-- operations, so a lease makes a collision VISIBLE AND AVOIDABLE — it is not a
-- mutex, and that limitation is deliberate and documented.
--
-- A crashed agent's lease expires rather than deadlocking the repo.
CREATE TABLE repo_leases (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id         uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  repo_id        uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  branch         text        NOT NULL,
  holder_user_id uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  -- Free-form label for the holding session, so a human reading `list_leases`
  -- can tell two of their own agents apart ('claude-code@laptop', 'ci-runner-3').
  holder_label   text,
  job_id         text,
  acquired_at    timestamptz NOT NULL DEFAULT now(),
  renewed_at     timestamptz NOT NULL DEFAULT now(),
  expires_at     timestamptz NOT NULL,
  released_at    timestamptz
);

-- At most one live lease per (repo, branch). Expiry is NOT in this predicate:
-- now() is not immutable and cannot appear in an index. `acquire_lease` reaps
-- expired rows inside the same transaction before inserting, so this partial
-- unique index is the correctness backstop and the reap is the liveness path.
CREATE UNIQUE INDEX repo_leases_live_key
  ON repo_leases (repo_id, branch)
  WHERE released_at IS NULL;

CREATE INDEX repo_leases_org_expiry_idx ON repo_leases (org_id, expires_at)
  WHERE released_at IS NULL;
