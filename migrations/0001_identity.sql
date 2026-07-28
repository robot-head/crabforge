-- Users, repositories, and access control.
--
-- Written as standard PostgreSQL. Where crabka's gres engine does not yet
-- support something, the workaround is marked TODO(gres:<feature>) and tracked
-- in docs/gres-gaps.md so it can be deleted when the feature lands.
--
-- Two structural notes that apply to every table here:
--
--   * Ids are UUIDv7 rendered as text. Being time-ordered, they double as
--     pagination cursors, so no list query needs a secondary sort key.
--   * Every hot query is narrowed first by an indexed equality column, because
--     gres indexes are single-column equality only. Lookup keys that would
--     naturally be composite (owner + name) are stored pre-joined and
--     pre-lowered instead.  TODO(gres:composite-index)

CREATE TABLE users (
  -- TODO(gres:primary-key): declare PRIMARY KEY when supported. Uniqueness is
  -- enforced by the command service, which is the only writer and holds a
  -- replay-built index of claimed names.
  user_id        text NOT NULL,
  username       text NOT NULL,
  username_lower text NOT NULL,
  email          text NOT NULL,
  -- argon2id PHC string. Never a reversible encoding of the password.
  password_hash  text NOT NULL,
  display_name   text,
  bio            text,
  state          text NOT NULL,          -- 'active' | 'deactivated'
                                         -- TODO(gres:check-constraints)
  created_at     timestamptz NOT NULL,
  updated_at     timestamptz NOT NULL
);
CREATE INDEX users_by_id ON users (user_id);
CREATE INDEX users_by_username_lower ON users (username_lower);

CREATE TABLE repos (
  repo_id         text NOT NULL,
  -- TODO(gres:foreign-keys): references users(user_id). Integrity is enforced
  -- upstream in the command service.
  owner_id        text NOT NULL,
  owner_name      text NOT NULL,         -- denormalized: avoids a join on every
                                         -- repo page render
  name            text NOT NULL,
  full_name_lower text NOT NULL,         -- 'owner/name', pre-lowered
  description     text,
  default_branch  text NOT NULL,
  visibility      text NOT NULL,         -- 'public' | 'private'
  created_at      timestamptz NOT NULL,
  updated_at      timestamptz NOT NULL,
  deleted         boolean NOT NULL
);
CREATE INDEX repos_by_id ON repos (repo_id);
CREATE INDEX repos_by_full_name_lower ON repos (full_name_lower);
CREATE INDEX repos_by_owner ON repos (owner_id);

CREATE TABLE repo_collaborators (
  collab_id  text NOT NULL,
  repo_id    text NOT NULL,
  user_id    text NOT NULL,
  username   text NOT NULL,
  role       text NOT NULL,              -- 'read' | 'write' | 'admin'
  created_at timestamptz NOT NULL
);
CREATE INDEX collaborators_by_repo ON repo_collaborators (repo_id);
CREATE INDEX collaborators_by_user ON repo_collaborators (user_id);

-- Where each projector has got to.
--
-- Updated in the same transaction as the rows it covers, which is what makes
-- projection exactly-once in effect: a crash between reading the log and
-- committing replays the batch, and a crash after committing does not.
CREATE TABLE projector_state (
  topic          text NOT NULL,
  partition      int4 NOT NULL,
  applied_offset int8 NOT NULL,
  updated_at     timestamptz NOT NULL
);
CREATE INDEX projector_state_by_topic ON projector_state (topic);
