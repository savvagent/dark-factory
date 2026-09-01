-- Metering and billing. The revenue model is usage-based with tier buckets, and
-- the billable unit is the MCP tool call — but NOT every tool call.
--
-- `watch` is a 30-second long poll that every connected agent calls
-- continuously. Billing it flat would charge an idle agent ~86,000 calls a month
-- for doing nothing and make bills impossible to predict. So each tool is
-- classified in code (df-billing::classify) as free (reads and polls) or
-- billable (work), and only billable calls consume the bucket.
--
-- Both kinds are recorded here regardless, so the classification can be repriced
-- later without having lost the history to reprice against.

CREATE TABLE usage_events (
  id         bigserial PRIMARY KEY,
  org_id     uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  user_id    uuid        REFERENCES users (id) ON DELETE SET NULL,
  tool       text        NOT NULL,
  billable   boolean     NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX usage_events_org_time_idx ON usage_events (org_id, created_at DESC);
CREATE INDEX usage_events_org_billable_idx ON usage_events (org_id, created_at)
  WHERE billable;

-- Rolled-up counters, incremented in the SAME transaction as the tool's own
-- work. A call that fails is therefore never billed, and a call that succeeds is
-- never billed twice — the counter and the effect commit or abort together.
CREATE TABLE org_period_usage (
  org_id         uuid        NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
  -- First day of the UTC billing month.
  period_start   date        NOT NULL,
  billable_count bigint      NOT NULL DEFAULT 0,
  total_count    bigint      NOT NULL DEFAULT 0,
  updated_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, period_start)
);

-- Plan definitions, in the database rather than in code so a bucket can be
-- adjusted (or an enterprise contract written) without a deploy.
CREATE TABLE plans (
  plan          org_plan PRIMARY KEY,
  display_name  text     NOT NULL,
  included_ops  bigint   NOT NULL,
  -- Free stops dead at the bucket; paid plans meter overage.
  hard_stop     boolean  NOT NULL DEFAULT false,
  monthly_cents integer  NOT NULL DEFAULT 0
);

INSERT INTO plans (plan, display_name, included_ops, hard_stop, monthly_cents) VALUES
  ('free',       'Free',         500, true,      0),
  ('team',       'Team',       10000, false,  2900),
  ('business',   'Business',  100000, false, 19900),
  ('enterprise', 'Enterprise', 1000000, false,     0);

CREATE TABLE subscriptions (
  org_id                 uuid PRIMARY KEY REFERENCES orgs (id) ON DELETE CASCADE,
  stripe_customer_id     text,
  stripe_subscription_id text,
  status                 text        NOT NULL DEFAULT 'active',
  -- Overrides plans.included_ops for a negotiated enterprise contract.
  included_ops_override  bigint,
  current_period_start   timestamptz,
  current_period_end     timestamptz,
  updated_at             timestamptz NOT NULL DEFAULT now()
);
