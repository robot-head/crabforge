//! The `pulls` read model.

use time::OffsetDateTime;
use tokio_postgres::Client;

use crate::{PageSize, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRecord {
    pub pr_id: String,
    pub repo_id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub author_id: String,
    pub author_name: String,
    pub state: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_oid: String,
    pub base_oid: String,
    /// The last trial merge, or `None` if nobody has run one.
    pub merge_check: Option<MergeCheck>,
    pub merge_commit_oid: Option<String>,
    pub merged_by_name: Option<String>,
    pub comment_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub merged_at: Option<OffsetDateTime>,
    pub closed_at: Option<OffsetDateTime>,
}

/// What a trial merge concluded, as reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    /// Not computed yet, or computed against commits that have since moved.
    Unknown,
    Clean,
    Conflict,
}

impl Mergeable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Clean => "clean",
            Self::Conflict => "conflict",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "clean" => Self::Clean,
            "conflict" => Self::Conflict,
            _ => Self::Unknown,
        }
    }
}

/// A trial merge, together with the two commits it was run on.
///
/// The verdict and its subject are one value on purpose. A trial merge is far
/// too expensive to repeat per page view, so the answer is cached — and a
/// cached answer about two commits is misleading the moment either branch
/// moves. Storing them apart makes "is this still true?" a question a reader
/// can forget to ask; storing them together makes it unavoidable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeCheck {
    /// `clean` or `conflict`, as written by the worker.
    pub verdict: String,
    /// The head commit this was computed for.
    pub head: String,
    /// The base commit this was computed for.
    pub base: String,
    /// Conflicting paths. Empty for a clean merge.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl MergeCheck {
    pub fn clean(head: impl Into<String>, base: impl Into<String>) -> Self {
        Self {
            verdict: Mergeable::Clean.as_str().to_string(),
            head: head.into(),
            base: base.into(),
            paths: Vec::new(),
        }
    }

    pub fn conflict(head: impl Into<String>, base: impl Into<String>, paths: Vec<String>) -> Self {
        Self {
            verdict: Mergeable::Conflict.as_str().to_string(),
            head: head.into(),
            base: base.into(),
            paths,
        }
    }
}

impl PullRecord {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    pub fn is_merged(&self) -> bool {
        self.state == "merged"
    }

    /// The trial-merge verdict, but only if it still describes this request.
    ///
    /// A check computed against commits either branch has since moved past
    /// reads as `Unknown`, because that is what it is worth.
    pub fn mergeability(&self) -> Mergeable {
        match &self.merge_check {
            Some(check) if check.head == self.head_oid && check.base == self.base_oid => {
                Mergeable::parse(&check.verdict)
            }
            _ => Mergeable::Unknown,
        }
    }

    /// The conflicting paths, if the check that found them is still current.
    pub fn conflicts(&self) -> &[String] {
        match &self.merge_check {
            Some(check) if check.head == self.head_oid && check.base == self.base_oid => {
                &check.paths
            }
            _ => &[],
        }
    }

    /// Whether the merge button should be offered.
    ///
    /// Only for an open pull request whose trial merge is known to be clean.
    /// `Unknown` deliberately does not qualify: offering a button that then
    /// fails is worse than a spinner.
    pub fn can_merge(&self) -> bool {
        self.is_open() && self.mergeability() == Mergeable::Clean
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub review_id: String,
    pub pr_id: String,
    pub repo_id: String,
    pub reviewer_id: String,
    pub reviewer_name: String,
    pub verdict: String,
    pub body: Option<String>,
    pub created_at: OffsetDateTime,
}

pub struct PullStore<'a> {
    client: &'a Client,
}

impl<'a> PullStore<'a> {
    pub fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// See `UserStore::upsert`. The branches a request was opened between are
    /// absent from the update: they are what it *is*, not what it currently
    /// says.
    pub async fn upsert(&self, pr: &PullRecord) -> Result<(), StoreError> {
        let merge_check = encode_check(pr.merge_check.as_ref())?;
        self.client
            .execute(
                "INSERT INTO pulls (pr_id, repo_id, number, title, body, author_id, \
                 author_name, state, source_branch, target_branch, head_oid, base_oid, \
                 merge_check, merge_commit_oid, merged_by_name, comment_count, created_at, \
                 updated_at, merged_at, closed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20) \
                 ON CONFLICT (pr_id) DO UPDATE SET \
                 title = excluded.title, body = excluded.body, state = excluded.state, \
                 head_oid = excluded.head_oid, base_oid = excluded.base_oid, \
                 merge_check = excluded.merge_check, \
                 merge_commit_oid = excluded.merge_commit_oid, \
                 merged_by_name = excluded.merged_by_name, \
                 comment_count = excluded.comment_count, updated_at = excluded.updated_at, \
                 merged_at = excluded.merged_at, closed_at = excluded.closed_at",
                &[
                    &pr.pr_id,
                    &pr.repo_id,
                    &pr.number,
                    &pr.title,
                    &pr.body,
                    &pr.author_id,
                    &pr.author_name,
                    &pr.state,
                    &pr.source_branch,
                    &pr.target_branch,
                    &pr.head_oid,
                    &pr.base_oid,
                    &merge_check,
                    &pr.merge_commit_oid,
                    &pr.merged_by_name,
                    &pr.comment_count,
                    &pr.created_at,
                    &pr.updated_at,
                    &pr.merged_at,
                    &pr.closed_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn by_id(&self, pr_id: &str) -> Result<Option<PullRecord>, StoreError> {
        let row = self
            .client
            .query_opt(&format!("{PULL_COLUMNS} WHERE pr_id = $1"), &[&pr_id])
            .await?;
        Ok(row.as_ref().map(row_to_pull))
    }

    pub async fn by_number(
        &self,
        repo_id: &str,
        number: i64,
    ) -> Result<Option<PullRecord>, StoreError> {
        let row = self
            .client
            .query_opt(
                &format!("{PULL_COLUMNS} WHERE repo_id = $1 AND number = $2"),
                &[&repo_id, &number],
            )
            .await?;
        Ok(row.as_ref().map(row_to_pull))
    }

    /// Open or closed pull requests, newest first.
    pub async fn list(
        &self,
        repo_id: &str,
        open: bool,
        limit: PageSize,
    ) -> Result<Vec<PullRecord>, StoreError> {
        // TODO(gres:parameterized-limit)
        let limit = *limit;
        // "Closed" in the list sense includes merged: both are done.
        let rows = if open {
            self.client
                .query(
                    &format!(
                        "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' \
                         ORDER BY number DESC LIMIT {limit}"
                    ),
                    &[&repo_id],
                )
                .await?
        } else {
            self.client
                .query(
                    &format!(
                        "{PULL_COLUMNS} WHERE repo_id = $1 AND state <> 'open' \
                         ORDER BY number DESC LIMIT {limit}"
                    ),
                    &[&repo_id],
                )
                .await?
        };
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Every open pull request targeting a branch.
    ///
    /// Used when that branch moves: each one's diff and mergeability are now
    /// stale and have to be recomputed.
    pub async fn open_targeting(
        &self,
        repo_id: &str,
        branch: &str,
    ) -> Result<Vec<PullRecord>, StoreError> {
        let rows = self
            .client
            .query(
                &format!(
                    "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' AND target_branch = $2"
                ),
                &[&repo_id, &branch],
            )
            .await?;
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Every open pull request whose source is a branch.
    pub async fn open_from(
        &self,
        repo_id: &str,
        branch: &str,
    ) -> Result<Vec<PullRecord>, StoreError> {
        let rows = self
            .client
            .query(
                &format!(
                    "{PULL_COLUMNS} WHERE repo_id = $1 AND state = 'open' AND source_branch = $2"
                ),
                &[&repo_id, &branch],
            )
            .await?;
        Ok(rows.iter().map(row_to_pull).collect())
    }

    /// Record a trial merge, unless the request has moved on without it.
    ///
    /// Returns whether it was stored. A trial merge takes long enough that a
    /// push can land while it runs, and the result would then be about history
    /// nobody is looking at any more — so the commits it was computed for are
    /// part of the `WHERE`. Filtering in the statement rather than reading
    /// first means there is no window between the check and the write.
    pub async fn record_check(
        &self,
        pr_id: &str,
        check: &MergeCheck,
        at: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let json = serde_json::to_value(check).map_err(StoreError::Json)?;
        let updated = self
            .client
            .execute(
                "UPDATE pulls SET merge_check = $2, updated_at = $3 \
                 WHERE pr_id = $1 AND head_oid = $4 AND base_oid = $5",
                &[&pr_id, &json, &at, &check.head, &check.base],
            )
            .await?;
        Ok(updated > 0)
    }

    /// Record a review, ignoring one already applied by an earlier replay.
    pub async fn insert_review(&self, review: &ReviewRecord) -> Result<(), StoreError> {
        self.client
            .execute(
                "INSERT INTO pr_reviews (review_id, pr_id, repo_id, reviewer_id, reviewer_name, \
                 verdict, body, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (review_id) DO NOTHING",
                &[
                    &review.review_id,
                    &review.pr_id,
                    &review.repo_id,
                    &review.reviewer_id,
                    &review.reviewer_name,
                    &review.verdict,
                    &review.body,
                    &review.created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn reviews(&self, pr_id: &str) -> Result<Vec<ReviewRecord>, StoreError> {
        let rows = self
            .client
            .query(
                "SELECT review_id, pr_id, repo_id, reviewer_id, reviewer_name, verdict, body, \
                 created_at FROM pr_reviews WHERE pr_id = $1 ORDER BY review_id ASC",
                &[&pr_id],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| ReviewRecord {
                review_id: row.get(0),
                pr_id: row.get(1),
                repo_id: row.get(2),
                reviewer_id: row.get(3),
                reviewer_name: row.get(4),
                verdict: row.get(5),
                body: row.get(6),
                created_at: row.get(7),
            })
            .collect())
    }
}

const PULL_COLUMNS: &str = "SELECT pr_id, repo_id, number, title, body, author_id, author_name, \
     state, source_branch, target_branch, head_oid, base_oid, merge_check, merge_commit_oid, \
     merged_by_name, comment_count, created_at, updated_at, merged_at, closed_at FROM pulls";

/// A `MergeCheck` as it goes into the jsonb column.
fn encode_check(check: Option<&MergeCheck>) -> Result<Option<serde_json::Value>, StoreError> {
    check
        .map(serde_json::to_value)
        .transpose()
        .map_err(StoreError::Json)
}

/// A `MergeCheck` as it comes back out.
///
/// A value that will not parse is treated as no check at all rather than
/// failing the read: the cost is a recomputed trial merge, where the
/// alternative is a pull request page that cannot be opened.
fn decode_check(value: Option<serde_json::Value>) -> Option<MergeCheck> {
    let value = value?;
    match serde_json::from_value(value) {
        Ok(check) => Some(check),
        Err(error) => {
            tracing::warn!(%error, "ignoring an unreadable merge check");
            None
        }
    }
}

fn row_to_pull(row: &tokio_postgres::Row) -> PullRecord {
    PullRecord {
        pr_id: row.get(0),
        repo_id: row.get(1),
        number: row.get(2),
        title: row.get(3),
        body: row.get(4),
        author_id: row.get(5),
        author_name: row.get(6),
        state: row.get(7),
        source_branch: row.get(8),
        target_branch: row.get(9),
        head_oid: row.get(10),
        base_oid: row.get(11),
        merge_check: decode_check(row.get(12)),
        merge_commit_oid: row.get(13),
        merged_by_name: row.get(14),
        comment_count: row.get(15),
        created_at: row.get(16),
        updated_at: row.get(17),
        merged_at: row.get(18),
        closed_at: row.get(19),
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn pull(state: &str, mergeable: &str) -> PullRecord {
        let check = match mergeable {
            "clean" => Some(MergeCheck::clean("a", "b")),
            "conflict" => Some(MergeCheck::conflict("a", "b", vec!["f.txt".into()])),
            _ => None,
        };
        pull_with(state, check)
    }

    fn pull_with(state: &str, merge_check: Option<MergeCheck>) -> PullRecord {
        let now = forge_types::now();
        PullRecord {
            pr_id: "p".into(),
            repo_id: "r".into(),
            number: 1,
            title: "t".into(),
            body: None,
            author_id: "u".into(),
            author_name: "octocat".into(),
            state: state.into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            head_oid: "a".into(),
            base_oid: "b".into(),
            merge_check,
            merge_commit_oid: None,
            merged_by_name: None,
            comment_count: 0,
            created_at: now,
            updated_at: now,
            merged_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn the_merge_button_is_offered_only_for_a_clean_open_request() {
        check!(pull("open", "clean").can_merge());

        // A conflicted one cannot be merged.
        check!(!pull("open", "conflict").can_merge());
        // Nor one whose mergeability nobody has computed yet: a button that
        // then fails is worse than waiting for the answer.
        check!(!pull("open", "unknown").can_merge());
        // Nor a finished one.
        check!(!pull("merged", "clean").can_merge());
        check!(!pull("closed", "clean").can_merge());
    }

    #[test]
    fn an_unrecognised_mergeability_reads_as_unknown() {
        // Rather than as mergeable, which would enable a button on a guess.
        check!(Mergeable::parse("something else") == Mergeable::Unknown);
        check!(Mergeable::parse("") == Mergeable::Unknown);
    }

    #[test]
    fn mergeability_round_trips_through_its_stored_form() {
        for value in [Mergeable::Unknown, Mergeable::Clean, Mergeable::Conflict] {
            check!(Mergeable::parse(value.as_str()) == value);
        }
    }

    #[test]
    fn a_check_computed_for_other_commits_does_not_count() {
        // The reason the verdict and the commits live in one value: a push
        // moves `head_oid`, and a "clean" answer about the commit before it
        // would offer a merge button for a merge nobody has tried.
        let mut pr = pull_with("open", Some(MergeCheck::clean("a", "b")));
        check!(pr.mergeability() == Mergeable::Clean);
        check!(pr.can_merge());

        pr.head_oid = "a2".into();
        check!(
            pr.mergeability() == Mergeable::Unknown,
            "a moved head should invalidate the check"
        );
        check!(!pr.can_merge());

        // And the same when the branch being merged into moves underneath.
        let mut pr = pull_with("open", Some(MergeCheck::clean("a", "b")));
        pr.base_oid = "b2".into();
        check!(pr.mergeability() == Mergeable::Unknown);
    }

    #[test]
    fn a_stale_conflict_list_is_not_shown() {
        // Sending someone to reconcile a file that no longer disagrees is worse
        // than showing nothing.
        let mut pr = pull_with(
            "open",
            Some(MergeCheck::conflict("a", "b", vec!["src/lib.rs".into()])),
        );
        check!(pr.conflicts() == ["src/lib.rs"]);

        pr.head_oid = "a2".into();
        check!(pr.conflicts().is_empty());
    }

    #[test]
    fn a_check_survives_its_json_encoding() {
        let check = MergeCheck::conflict("head", "base", vec!["a.txt".into(), "b/c.txt".into()]);
        let encoded = encode_check(Some(&check)).unwrap();
        check!(decode_check(encoded) == Some(check));

        check!(encode_check(None).unwrap().is_none());
        check!(decode_check(None).is_none());
    }

    #[test]
    fn an_unreadable_check_reads_as_no_check() {
        // Rather than failing the whole query: the cost is one recomputed trial
        // merge, and the alternative is a page that will not open.
        let junk = serde_json::json!({"verdict": 7});
        check!(decode_check(Some(junk)).is_none());
    }
}
