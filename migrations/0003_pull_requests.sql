-- Pull requests.
--
-- A pull request shares the issue number sequence, as GitHub does: `#7` refers
-- to one thing in a repository whether it is an issue or a pull request, so
-- cross-references cannot be ambiguous.

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
