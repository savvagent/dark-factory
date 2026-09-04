-- Two concurrent webhook deliveries for the same freshly-labelled issue can
-- both run inbound_decision, both see "no existing job" for the ticket_ref,
-- and both call create_from_ticket, producing two jobs for one ticket. A
-- partial unique index closes that race at the database, which is the only
-- place a race between two application transactions can actually be closed.
--
-- Partial (not a plain unique constraint) and scoped to non-terminal
-- statuses only, so a ticket that closes and later reopens can still get a
-- fresh job — jobs.rs's own get_job_by_ticket_for_repo already tolerates
-- multiple historical jobs per ticket_ref (newest-wins), and this index must
-- not contradict that by forbidding more than one ever.
CREATE UNIQUE INDEX jobs_org_repo_tracker_ticket_open_idx
    ON jobs (org_id, repo_id, tracker, ticket_ref)
    WHERE ticket_ref IS NOT NULL AND status IN ('pending', 'in-progress');
