-- Sessions, access tokens, and issues.
--
-- Two different kinds of table live here, and the difference matters.
--
-- `web_sessions` and `access_tokens.last_used_at` are *operational* state: they
-- describe a browser tab, not something that happened to the project. They are
-- written directly by the web tier and have no place in the event log. Losing
-- them logs people out, which is a nuisance rather than data loss.
--
-- Everything else is a projection of the log and is rebuilt by replaying it.
--
-- The two sets are written by different processes, so they are deliberately
-- disjoint: no row is written by both the projector and the web tier.

-- Operational: a logged-in browser.
--
-- The cookie itself is never stored. What is stored is its SHA-256, so a
-- database leak does not hand out live sessions.
CREATE TABLE web_sessions (
  session_hash text NOT NULL,
  user_id      text NOT NULL,
  created_at   timestamptz NOT NULL,
  expires_at   timestamptz NOT NULL
);
CREATE INDEX web_sessions_by_hash ON web_sessions (session_hash);
CREATE INDEX web_sessions_by_user ON web_sessions (user_id);

-- Projected: personal access tokens, for git over HTTP and the API.
--
-- Also stored as a hash, for the same reason. Creation and revocation are
-- domain events — an account's credentials changing is part of its history —
-- but `last_used_at` is written directly, because recording every use as an
-- event would put a write on the log for every git fetch.
CREATE TABLE access_tokens (
  token_id     text NOT NULL,
  user_id      text NOT NULL,
  name         text NOT NULL,
  token_hash   text NOT NULL,
  scopes       text NOT NULL,          -- space-separated
  created_at   timestamptz NOT NULL,
  expires_at   timestamptz,
  revoked_at   timestamptz,
  last_used_at timestamptz
);
CREATE INDEX access_tokens_by_hash ON access_tokens (token_hash);
CREATE INDEX access_tokens_by_user ON access_tokens (user_id);
CREATE INDEX access_tokens_by_id ON access_tokens (token_id);

-- Projected: issues.
--
-- `number` is per repository and allocated by the command service, which is the
-- only writer and therefore the only thing that can hand out a sequence without
-- a gap or a duplicate.
CREATE TABLE issues (
  issue_id      text NOT NULL,
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
  closed_at     timestamptz
);
CREATE INDEX issues_by_repo ON issues (repo_id);
CREATE INDEX issues_by_id ON issues (issue_id);

-- Projected: comments on issues.
--
-- Ordered by `comment_id`, which is a UUIDv7 and therefore chronological. That
-- is what lets a conversation page be keyset-paginated without a sort column.
CREATE TABLE issue_comments (
  comment_id  text NOT NULL,
  issue_id    text NOT NULL,
  repo_id     text NOT NULL,
  author_id   text NOT NULL,
  author_name text NOT NULL,
  body        text NOT NULL,
  created_at  timestamptz NOT NULL,
  updated_at  timestamptz NOT NULL
);
CREATE INDEX issue_comments_by_issue ON issue_comments (issue_id);

-- Projected: per-repository counters.
--
-- Maintained as columns because `count(*) WHERE state = 'open'` runs on every
-- page that shows a tab badge, and gres has no composite index to make it cheap.
-- TODO(gres:composite-index)
CREATE TABLE repo_counters (
  repo_id       text NOT NULL,
  open_issues   int8 NOT NULL,
  closed_issues int8 NOT NULL
);
CREATE INDEX repo_counters_by_repo ON repo_counters (repo_id);
