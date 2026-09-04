-- 0015_jobs_ticket_ref_uniqueness.sql's partial unique index enforces at most
-- one *open* (non-terminal) job per (repo, tracker, ticket_ref), scoped to
-- 'pending'/'in-progress'. An active job is still open — it must keep
-- blocking a second job for the same ticket, or a webhook race could create
-- a duplicate for a ticket whose job has moved to 'active'. 0015 is never
-- edited (it is already applied); the index is dropped and recreated here
-- with the same shape, widened to include 'active'. Split into its own
-- migration file after 0016 so the new enum value it references is already
-- committed — see 0016's comment.
DROP INDEX jobs_org_repo_tracker_ticket_open_idx;
CREATE UNIQUE INDEX jobs_org_repo_tracker_ticket_open_idx
    ON jobs (org_id, repo_id, tracker, ticket_ref)
    WHERE ticket_ref IS NOT NULL AND status IN ('pending', 'in-progress', 'active');
