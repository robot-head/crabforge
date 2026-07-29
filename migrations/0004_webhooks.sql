-- Webhooks, and the record of what was delivered.
--
-- A webhook's *configuration* is domain history: someone decided this project
-- should tell that URL about pushes, and who decided it matters. Individual
-- *deliveries* are not — they are operational records with a retention horizon,
-- kept so a maintainer can see why an integration is not working.

CREATE TABLE webhooks (
  webhook_id  text PRIMARY KEY,
  repo_id     text NOT NULL,
  url         text NOT NULL,
  -- The signing secret, stored as written. Unlike a password or a session, this
  -- one has to be replayable: signatures are computed from it on every
  -- delivery, so a digest would be useless.
  secret      text NOT NULL,
  -- Subscribed event types: exact (`issue.opened`), prefix (`issue.*`), or the
  -- single element `*` for everything.
  events      text[] NOT NULL,
  active      boolean NOT NULL,
  created_at  timestamptz NOT NULL,
  updated_at  timestamptz NOT NULL
);
CREATE INDEX webhooks_by_repo ON webhooks (repo_id);

-- One attempt at one delivery.
CREATE TABLE webhook_deliveries (
  delivery_id  text PRIMARY KEY,
  webhook_id   text NOT NULL,
  repo_id      text NOT NULL,
  event_type   text NOT NULL,
  -- The CloudEvents id of the event that caused this, so a receiver's
  -- deduplication key is visible from our side too.
  event_id     text NOT NULL,
  attempt      int8 NOT NULL,
  -- 'pending' | 'delivered' | 'failed' | 'dead'
  status       text NOT NULL,
  status_code  int8,
  error        text,
  duration_ms  int8,
  created_at   timestamptz NOT NULL
);
CREATE INDEX deliveries_by_webhook ON webhook_deliveries (webhook_id);
