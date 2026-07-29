-- The crabforge schema.
--
-- One file, edited in place. Crabforge has no deployment to migrate, so an
-- incremental migration would only add a second place to look for what a table
-- is, describing a state no database has ever been in. When something is
-- deployed, this file freezes and `0002_*.sql` begins.
--
-- The footgun that buys: editing this file does NOT migrate a database that has
-- already applied version 1. The runner skips any version already in the
-- ledger, so an existing dev database silently keeps the old schema and the
-- mismatch surfaces later as a confusing query error. Run `just dev-reset`
-- after editing. This is tolerable only while nothing is deployed.
--
-- Written as standard PostgreSQL. Where crabka's gres engine does not yet
-- support something, the workaround is marked TODO(gres:<feature>) and tracked
-- in docs/gres-gaps.md so it can be deleted when the feature lands.
--
--
-- ## What is in here, and what owns it
--
-- Two kinds of table live in this schema, and the difference decides who may
-- write to them. Each table below says which it is.
--
-- **Projections** are derived state, written only by the projector and rebuilt
-- by replaying the log from offset zero. Dropping them all and re-projecting is
-- not a theoretical property — it is the disaster-recovery procedure.
--
-- **Operational** tables describe a session or an attempt rather than something
-- that happened to the project. They are written directly by whichever service
-- observes them, and have no place in the event log: recording a page view or a
-- delivery retry as domain history would bury the history that matters. Losing
-- them logs people out or forgets why an integration failed, which is a
-- nuisance rather than data loss.
--
-- The split is usually by table but not always by row. `access_tokens` is
-- projected from the log except for `last_used_at`, which the web tier writes
-- on every authenticated request — recording that as an event would put a write
-- on the log for every git fetch. It is the one table two writers touch, and
-- they are confined to disjoint columns of it deliberately, so the two never
-- conflict over the same value.
--
--
-- ## Conventions that apply throughout
--
--   * Ids are UUIDv7 rendered as text. Being time-ordered, they double as
--     pagination cursors, so no list query needs a secondary sort key.
--   * Every hot query is narrowed first by an indexed equality column. Keys
--     that would naturally be composite (owner + name) are stored pre-joined
--     and pre-lowered, because gres index *scans* are single-column equality
--     even though composite constraints are enforced.
--     TODO(gres:composite-index)
--   * Primary keys and unique constraints are declared even where they should
--     be unreachable. The command service is the only writer of the log and
--     holds a replay-built index of what has been claimed, and the projector
--     only ever replays what that service already accepted — so these should
--     never fire. They are here so that if either has a bug, the result is a
--     rejected write rather than two rows that disagree about who owns a name.
--   * No secondary index duplicates a single-column primary key or unique
--     constraint, because those indexes serve reads and not only enforcement
--     (measured on gres). A *composite* unique constraint is different: it does
--     not answer a lookup on its first column alone, which is why
--     `repo_collaborators` still carries its own `repo_id` and `user_id`
--     indexes alongside `UNIQUE (repo_id, user_id)`.
--   * Columns holding an enum-like string are unconstrained text throughout;
--     the values are listed in a comment and validated in Rust.
--     TODO(gres:check-constraints)


-- ── Accounts and repositories ───────────────────────────────────────────────

-- Projected.
CREATE TABLE users (
  user_id        text PRIMARY KEY,
  username       text NOT NULL,
  username_lower text NOT NULL UNIQUE,
  email          text NOT NULL,
  -- argon2id PHC string. Never a reversible encoding of the password.
  password_hash  text NOT NULL,
  display_name   text,
  bio            text,
  state          text NOT NULL,          -- 'active' | 'deactivated'
  created_at     timestamptz NOT NULL,
  updated_at     timestamptz NOT NULL
);

-- Projected.
CREATE TABLE repos (
  repo_id         text PRIMARY KEY,
  -- TODO(gres:foreign-keys): references users(user_id). Integrity is enforced
  -- upstream in the command service. The same applies to every other column
  -- here that names a row in another table, and is not repeated.
  owner_id        text NOT NULL,
  owner_name      text NOT NULL,         -- denormalized: avoids a join on every
                                         -- repo page render
  name            text NOT NULL,
  full_name_lower text NOT NULL UNIQUE,  -- 'owner/name', pre-lowered
  description     text,
  default_branch  text NOT NULL,
  visibility      text NOT NULL,         -- 'public' | 'private'
  created_at      timestamptz NOT NULL,
  updated_at      timestamptz NOT NULL,
  deleted         boolean NOT NULL
);
CREATE INDEX repos_by_owner ON repos (owner_id);

-- Projected.
CREATE TABLE repo_collaborators (
  collab_id  text PRIMARY KEY,
  repo_id    text NOT NULL,
  user_id    text NOT NULL,
  username   text NOT NULL,
  role       text NOT NULL,              -- 'read' | 'write' | 'admin'
  created_at timestamptz NOT NULL,
  -- Someone is a collaborator on a repository once. A second grant has to
  -- replace the first, because two rows would make the effective permission
  -- depend on which one a query happened to read.
  UNIQUE (repo_id, user_id)
);
CREATE INDEX collaborators_by_repo ON repo_collaborators (repo_id);
CREATE INDEX collaborators_by_user ON repo_collaborators (user_id);


-- ── Authentication ──────────────────────────────────────────────────────────
--
-- Neither a session cookie nor a token is stored. What is stored is its
-- SHA-256, and lookups hash the presented credential and compare. A database
-- leak then yields nothing that can be replayed, and the fixed-width hash is
-- also a better index key than a variable-length secret.

-- Operational: a logged-in browser.
CREATE TABLE web_sessions (
  session_hash text PRIMARY KEY,
  user_id      text NOT NULL,
  created_at   timestamptz NOT NULL,
  expires_at   timestamptz NOT NULL
);
CREATE INDEX web_sessions_by_user ON web_sessions (user_id);

-- Projected, except `last_used_at`. Personal access tokens, for git over HTTP
-- and the API. Creation and revocation are domain events — an account's
-- credentials changing is part of its history — while the last-used stamp is
-- written directly by the web tier; see the header.
CREATE TABLE access_tokens (
  token_id     text PRIMARY KEY,
  user_id      text NOT NULL,
  name         text NOT NULL,
  -- Unique because two rows sharing a hash would mean one presented token
  -- authenticating as whichever account a query happened to read first.
  token_hash   text NOT NULL UNIQUE,
  scopes       text[] NOT NULL,
  created_at   timestamptz NOT NULL,
  expires_at   timestamptz,
  revoked_at   timestamptz,
  last_used_at timestamptz
);
CREATE INDEX access_tokens_by_user ON access_tokens (user_id);


-- ── Issues ──────────────────────────────────────────────────────────────────

-- Projected.
--
-- `number` is per repository and allocated by the command service, which is the
-- only writer and therefore the only thing that can hand out a sequence without
-- a gap or a duplicate. Declaring it unique means a bug there surfaces as a
-- rejected write rather than as two issues both answering to `#7`.
CREATE TABLE issues (
  issue_id      text PRIMARY KEY,
  repo_id       text NOT NULL,
  number        int8 NOT NULL,
  title         text NOT NULL,
  body          text,
  author_id     text NOT NULL,
  author_name   text NOT NULL,         -- denormalized: avoids a join per row
  state         text NOT NULL,         -- 'open' | 'closed'
  comment_count int8 NOT NULL,
  created_at    timestamptz NOT NULL,
  updated_at    timestamptz NOT NULL,
  closed_at     timestamptz,
  UNIQUE (repo_id, number)
);
CREATE INDEX issues_by_repo ON issues (repo_id);

-- Projected. Ordered by `comment_id`, which is a UUIDv7 and therefore
-- chronological — that is what lets a conversation page be keyset-paginated
-- without a sort column.
CREATE TABLE issue_comments (
  comment_id  text PRIMARY KEY,
  issue_id    text NOT NULL,
  repo_id     text NOT NULL,
  author_id   text NOT NULL,
  author_name text NOT NULL,
  body        text NOT NULL,
  created_at  timestamptz NOT NULL,
  updated_at  timestamptz NOT NULL
);
CREATE INDEX issue_comments_by_issue ON issue_comments (issue_id);

-- Projected. Per-repository counters, maintained as columns because
-- `count(*) WHERE state = 'open'` runs on every page that shows a tab badge,
-- and a gres index scan is single-column equality only — the state predicate
-- would be a filter over every issue in the repo.
-- TODO(gres:composite-index)
CREATE TABLE repo_counters (
  repo_id       text PRIMARY KEY,
  open_issues   int8 NOT NULL,
  closed_issues int8 NOT NULL
);


-- ── Pull requests ───────────────────────────────────────────────────────────
--
-- A pull request shares the issue number sequence, as GitHub does: `#7` refers
-- to one thing in a repository whether it is an issue or a pull request, so
-- cross-references cannot be ambiguous. The command service allocates from the
-- one sequence; `UNIQUE (repo_id, number)` here and on `issues` can each only
-- enforce their own half of that, since the constraint cannot span two tables.

-- Projected.
CREATE TABLE pulls (
  pr_id            text PRIMARY KEY,
  repo_id          text NOT NULL,
  number           int8 NOT NULL,
  title            text NOT NULL,
  body             text,
  author_id        text NOT NULL,
  author_name      text NOT NULL,
  state            text NOT NULL,        -- 'open' | 'closed' | 'merged'
  source_branch    text NOT NULL,
  target_branch    text NOT NULL,
  head_oid         text NOT NULL,
  base_oid         text NOT NULL,
  -- What the last trial merge concluded, and the commits it concluded it for:
  --
  --   {"verdict": "clean" | "conflict", "head": <oid>, "base": <oid>,
  --    "paths": [<conflicting path>, ...]}
  --
  -- NULL means nobody has looked yet. A trial merge is far too expensive to run
  -- on every page view, so the answer is stored — but an answer about two
  -- commits is worthless without knowing which two, and a branch moves. Keeping
  -- the verdict and its subject in one value is what makes them impossible to
  -- read apart: a reader compares the stamped commits against the current ones
  -- and treats a mismatch as "not computed yet".
  merge_check      jsonb,
  merge_commit_oid text,
  merged_by_name   text,
  comment_count    int8 NOT NULL,
  created_at       timestamptz NOT NULL,
  updated_at       timestamptz NOT NULL,
  merged_at        timestamptz,
  closed_at        timestamptz,
  UNIQUE (repo_id, number)
);
CREATE INDEX pulls_by_repo ON pulls (repo_id);

-- Projected.
CREATE TABLE pr_reviews (
  review_id     text PRIMARY KEY,
  pr_id         text NOT NULL,
  repo_id       text NOT NULL,
  reviewer_id   text NOT NULL,
  reviewer_name text NOT NULL,
  verdict       text NOT NULL,          -- 'approve' | 'request_changes' | 'comment'
  body          text,
  created_at    timestamptz NOT NULL
);
CREATE INDEX pr_reviews_by_pr ON pr_reviews (pr_id);


-- ── Webhooks ────────────────────────────────────────────────────────────────
--
-- A webhook's *configuration* is domain history: someone decided this project
-- should tell that URL about pushes, and who decided it matters. Individual
-- *deliveries* are not — they are operational records with a retention horizon,
-- kept so a maintainer can see why an integration is not working.

-- Projected.
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

-- Operational. One attempt at one delivery, written by the deliverer.
CREATE TABLE webhook_deliveries (
  delivery_id  text PRIMARY KEY,
  webhook_id   text NOT NULL,
  repo_id      text NOT NULL,
  event_type   text NOT NULL,
  -- The CloudEvents id of the event that caused this, so a receiver's
  -- deduplication key is visible from our side too.
  event_id     text NOT NULL,          -- a CloudEvents id, not a row anywhere
  attempt      int8 NOT NULL,
  status       text NOT NULL,          -- 'pending' | 'delivered' | 'failed' | 'dead'
  status_code  int8,
  error        text,
  duration_ms  int8,
  created_at   timestamptz NOT NULL
);
CREATE INDEX deliveries_by_webhook ON webhook_deliveries (webhook_id);


-- ── Projection bookkeeping ──────────────────────────────────────────────────

-- The projector's own bookkeeping: neither projected nor operational, since it
-- is what makes projection possible. Where each projector has got to.
--
-- Updated in the same transaction as the rows it covers, which is what makes
-- projection exactly-once in effect: a crash between reading the log and
-- committing replays the batch, and a crash after committing does not.
CREATE TABLE projector_state (
  topic          text NOT NULL,
  partition      int4 NOT NULL,
  applied_offset int8 NOT NULL,
  updated_at     timestamptz NOT NULL,
  PRIMARY KEY (topic, partition)
);
