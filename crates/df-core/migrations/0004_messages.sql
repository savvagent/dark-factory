-- The shared agent-to-agent message channel, plus each member's read cursor.
-- This is coordination chatter ("I'm taking the auth refactor, don't touch
-- crates/auth"), not a payload transport: bodies are bounded at the tool
-- boundary and every unread message is re-served on every inbox read.

CREATE TYPE message_kind AS ENUM ('note', 'request', 'response');

-- Who typed a message. Both a human in the console and their agent session
-- authenticate as the same user, so this is a rendering hint, not a security
-- claim — the authoritative sender_user_id is always set server-side from the
-- authenticated principal and can never be supplied by the client.
CREATE TYPE sender_kind AS ENUM ('agent', 'human');

CREATE TABLE messages (
  id               bigserial PRIMARY KEY,
  org_id           uuid         NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  created_at       timestamptz  NOT NULL DEFAULT now(),

  sender_user_id   uuid         NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  sender_label     text,
  sender_kind      sender_kind  NOT NULL DEFAULT 'agent',

  -- NULL recipient is a broadcast to the whole org (or team, when team_id is set).
  recipient_user_id uuid        REFERENCES users (id) ON DELETE CASCADE,
  team_id          uuid         REFERENCES teams (id) ON DELETE SET NULL,

  kind             message_kind NOT NULL DEFAULT 'note',
  body             text         NOT NULL,

  -- Optional coordination context.
  repo_id          uuid         REFERENCES repos (id) ON DELETE SET NULL,
  job_id           text,
  in_reply_to      bigint       REFERENCES messages (id) ON DELETE SET NULL,

  FOREIGN KEY (org_id, job_id) REFERENCES jobs (org_id, id) ON DELETE SET NULL
);

CREATE INDEX messages_org_id_idx ON messages (org_id, id DESC);
CREATE INDEX messages_recipient_idx ON messages (org_id, recipient_user_id, id DESC)
  WHERE recipient_user_id IS NOT NULL;

-- Per-member read cursor. `unread` is "id > cursor AND not sent by me".
CREATE TABLE message_cursors (
  org_id       uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  last_read_id bigint      NOT NULL DEFAULT 0,
  updated_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, user_id)
);

-- Messages wake `watch` through the same single-channel fan-out as jobs. The
-- payload carries the sender so a caller's own send does not wake its own long
-- poll — otherwise every self-send costs the sender a spurious inbox refetch.
CREATE OR REPLACE FUNCTION notify_message() RETURNS trigger AS $$
BEGIN
  PERFORM pg_notify(
    'df_changes',
    json_build_object(
      'kind',   'message',
      'org',    NEW.org_id,
      'id',     NEW.id::text,
      'op',     TG_OP,
      'sender', NEW.sender_user_id
    )::text
  );
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER messages_notify
  AFTER INSERT ON messages
  FOR EACH ROW EXECUTE FUNCTION notify_message();
