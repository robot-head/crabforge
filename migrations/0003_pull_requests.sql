-- Pull requests.
--
-- A pull request shares the issue number sequence, as GitHub does: `#7` refers
-- to one thing in a repository whether it is an issue or a pull request, so
-- cross-references cannot be ambiguous.

CREATE TABLE pulls (
  pr_id            text NOT NULL,
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
  -- 'unknown' until the mergeability worker has looked, then 'clean' or
  -- 'conflict'. Stored rather than computed per request because a trial merge
  -- is far too expensive to run on every page view.
  mergeable        text NOT NULL,
  merge_commit_oid text,
  merged_by_name   text,
  comment_count    int8 NOT NULL,
  created_at       timestamptz NOT NULL,
  updated_at       timestamptz NOT NULL,
  merged_at        timestamptz,
  closed_at        timestamptz
);
CREATE INDEX pulls_by_repo ON pulls (repo_id);
CREATE INDEX pulls_by_id ON pulls (pr_id);

-- Which files conflict, when they do.
--
-- Recorded against the exact pair of commits it was computed for: a stale
-- conflict list shown against a moved branch would send someone to reconcile a
-- file that no longer disagrees.
CREATE TABLE pr_conflicts (
  row_id           text NOT NULL,
  pr_id            text NOT NULL,
  path             text NOT NULL,
  computed_for_head text NOT NULL,
  computed_for_base text NOT NULL
);
CREATE INDEX pr_conflicts_by_pr ON pr_conflicts (pr_id);

CREATE TABLE pr_reviews (
  review_id     text NOT NULL,
  pr_id         text NOT NULL,
  repo_id       text NOT NULL,
  reviewer_id   text NOT NULL,
  reviewer_name text NOT NULL,
  verdict       text NOT NULL,          -- 'approve' | 'request_changes' | 'comment'
  body          text,
  created_at    timestamptz NOT NULL
);
CREATE INDEX pr_reviews_by_pr ON pr_reviews (pr_id);
