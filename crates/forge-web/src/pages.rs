//! The templates, as Rust types.
//!
//! ## The escaping rule
//!
//! askama escapes every `{{ value }}` by default. Pre-rendered HTML — a
//! highlighted file, rendered markdown — must *not* be escaped again, and the
//! way to say so is [`askama::filters::Safe`].
//!
//! House rule: `|safe` never appears in a template. Wrapping in `Safe` here
//! means the decision to trust a string is made in Rust, at the point where the
//! string was produced, where a reviewer can see it and grep for it. A `|safe`
//! buried in markup is the same decision made somewhere nobody audits.

use askama::Template;
use askama::filters::Safe;
use askama_web::WebTemplate;

use crate::state::MaybeViewer;

/// Fields every page needs: who is looking, and a token for their forms.
pub struct Chrome {
    pub viewer: MaybeViewer,
    pub csrf: String,
}

/// One row in a directory listing.
pub struct EntryView {
    pub name: String,
    pub url: String,
    /// The mark in the first column.
    pub icon: &'static str,
    /// What that mark means, for a reader who is not looking at it.
    pub kind: &'static str,
    pub size: String,
}

/// A rendered README.
pub struct ReadmeView {
    pub name: String,
    pub html: Safe<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/tree.html")]
pub struct TreePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub revision: String,
    pub path: String,
    pub parent_url: String,
    pub clone_url: String,
    pub empty: bool,
    pub entries: Vec<EntryView>,
    /// The tip of the revision being browsed. Absent on an empty repository,
    /// and only ever the one commit: a per-file "last changed" column is a
    /// `git log` for every row, which is not worth the wait on a listing.
    pub latest: Option<CommitView>,
    pub readme: Option<ReadmeView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/blob.html")]
pub struct BlobPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub revision: String,
    pub path: String,
    pub size: String,
    pub binary: bool,
    /// Already-escaped HTML from the highlighter.
    pub highlighted: Safe<String>,
}

/// One commit in a history listing.
pub struct CommitView {
    pub oid: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub initials: String,
    pub when: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/commits.html")]
pub struct CommitsPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub revision: String,
    pub commits: Vec<CommitView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/commit.html")]
pub struct CommitPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub oid: String,
    pub summary: String,
    pub body: Option<String>,
    pub author: String,
    pub initials: String,
    pub when: String,
    /// The raw unified diff. Escaped by askama, deliberately: it is git's
    /// output, not markup.
    pub diff: String,
}

/// One row in an issue list.
pub struct IssueRow {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub comments: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "issues/list.html")]
pub struct IssuesPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub closed_issues: i64,
    pub showing_open: bool,
    pub can_write: bool,
    pub issues: Vec<IssueRow>,
}

/// One comment in a conversation.
pub struct CommentView {
    pub author: String,
    pub initials: String,
    pub body: Safe<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "issues/detail.html")]
pub struct IssuePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub number: i64,
    pub title: String,
    pub open: bool,
    pub author: String,
    pub author_initials: String,
    pub body: Safe<String>,
    pub comments: Vec<CommentView>,
    pub can_write: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "issues/new.html")]
pub struct NewIssuePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
}

/// One row in a pull request list.
pub struct PullRow {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub source_branch: String,
    pub target_branch: String,
    pub merged: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "pulls/list.html")]
pub struct PullsPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub showing_open: bool,
    pub open_count: i64,
    pub closed_count: i64,
    pub pulls: Vec<PullRow>,
}

/// One review on a pull request.
pub struct ReviewView {
    pub reviewer: String,
    pub initials: String,
    pub verdict_label: String,
    pub verdict_class: String,
    pub body: Option<Safe<String>>,
}

#[derive(Template, WebTemplate)]
#[template(path = "pulls/detail.html")]
pub struct PullDetailPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub tab: &'static str,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
    pub number: i64,
    pub title: String,
    pub state_label: String,
    pub state_class: String,
    pub open: bool,
    pub merged: bool,
    pub mergeable: bool,
    pub conflicted: bool,
    pub conflicts: Vec<String>,
    pub author: String,
    pub author_initials: String,
    pub body: Option<Safe<String>>,
    pub source_branch: String,
    pub target_branch: String,
    pub head_oid: String,
    /// The head commit, abbreviated. The full one still goes in the merge
    /// form's hidden field — the short one is only ever for reading.
    pub head_short: String,
    pub merge_commit: String,
    pub merged_by: Option<String>,
    pub commit_count: i64,
    pub reviews: Vec<ReviewView>,
    /// Crab Actions runs for this pull request's head commit.
    pub checks: Vec<CheckView>,
    /// Whether every check that ran passed. False while any is still running,
    /// so a merge box cannot show green on an unfinished build.
    pub checks_passed: bool,
    /// git's own diff output. Escaped by askama, deliberately.
    pub diff: String,
    pub can_write: bool,
}

/// One CI run against a pull request's head commit.
pub struct CheckView {
    pub name: String,
    pub status: String,
    /// A CSS class, so the template does not branch on status text.
    pub status_class: String,
    /// A mark beside the status word. The palette is mono, so the glyph is the
    /// second channel and the word is the first — neither is a colour.
    pub glyph: &'static str,
    pub number: i64,
    pub jobs: Vec<CheckJobView>,
}

/// One job of a checked run.
pub struct CheckJobView {
    pub name: String,
    pub status: String,
    pub status_class: String,
    pub glyph: &'static str,
}

/// One repository on a profile.
pub struct RepoRow {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "profile.html")]
pub struct ProfilePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub username: String,
    pub initials: String,
    pub repos: Vec<RepoRow>,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/login.html")]
pub struct LoginPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/register.html")]
pub struct RegisterPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub status: u16,
    pub message: String,
}

/// A one- or two-letter monogram for a name.
///
/// The forge stores no avatars, and it is not going to fetch one from a
/// third party to draw its own chrome, so an identity is a letterform. Two
/// letters from two words where there are two, otherwise the first two of the
/// one word — which is what a reader recognises at 22 pixels.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    let letters: String = match words.as_slice() {
        [] => return "?".to_string(),
        [one] => one.chars().take(2).collect(),
        [first, second, ..] => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
    };
    letters.to_lowercase()
}

/// Abbreviate an object id for reading. The full one still travels in forms.
pub fn short_oid(oid: &str) -> String {
    oid.chars().take(7).collect()
}

/// Render a byte count for a listing.
pub fn human_size(bytes: Option<u64>) -> String {
    bytes.map_or_else(String::new, |b| forge_types::ByteSize::bytes(b).human())
}

/// The mark drawn beside a tree entry.
///
/// A glyph rather than a word, because the column sits in front of every row of
/// every listing and three letters of "dir" is three letters of noise. The word
/// is still in the markup — see [`kind_for`] — just not in the ink.
pub fn icon_for(kind: forge_git::EntryKind) -> &'static str {
    use forge_git::EntryKind;
    match kind {
        EntryKind::Directory => "▸",
        EntryKind::Symlink => "↳",
        EntryKind::Submodule => "◈",
        EntryKind::Executable => "▹",
        EntryKind::File => "·",
    }
}

/// What a tree entry is, spelled out for a screen reader.
pub fn kind_for(kind: forge_git::EntryKind) -> &'static str {
    use forge_git::EntryKind;
    match kind {
        EntryKind::Directory => "directory",
        EntryKind::Symlink => "symlink",
        EntryKind::Submodule => "submodule",
        EntryKind::Executable => "executable file",
        EntryKind::File => "file",
    }
}

/// Render a Unix timestamp as something a person reads.
///
/// Relative for anything recent, absolute past a week: "3 days ago" is what
/// people want for recent activity, and a date is what they want for history.
pub fn relative_time(unix_seconds: i64) -> String {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let delta = now.saturating_sub(unix_seconds);

    match delta {
        d if d < 60 => "just now".to_string(),
        d if d < 3600 => format!("{} minutes ago", d / 60),
        d if d < 86_400 => format!("{} hours ago", d / 3600),
        d if d < 604_800 => format!("{} days ago", d / 86_400),
        _ => time::OffsetDateTime::from_unix_timestamp(unix_seconds)
            .map(|t| format!("{} {} {}", t.day(), month_name(t.month()), t.year()))
            .unwrap_or_else(|_| "unknown".to_string()),
    }
}

fn month_name(month: time::Month) -> &'static str {
    use time::Month::*;
    match month {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn sizes_render_for_files_and_stay_blank_for_directories() {
        check!(human_size(Some(2048)) == "2 KiB");
        check!(human_size(None).is_empty());
    }

    #[test]
    fn recent_times_are_relative_and_old_ones_are_dated() {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        check!(relative_time(now) == "just now");
        check!(relative_time(now - 3600) == "1 hours ago");
        check!(relative_time(now - 86_400 * 3) == "3 days ago");
        // Past a week, an absolute date is more use than "412 days ago".
        let old = relative_time(now - 86_400 * 400);
        check!(old.contains("20"), "expected a year in {old}");
    }

    #[test]
    fn a_monogram_is_two_letters_however_the_name_is_spelled() {
        check!(initials("octocat") == "oc");
        check!(initials("Mira Vance") == "mv");
        check!(initials("refactor-agent") == "ra");
        check!(initials("j") == "j");
        // Names are user-supplied, so none of these may panic or leak markup.
        check!(initials("") == "?");
        check!(initials("<script>") == "sc");
        check!(initials("  ") == "?");
    }

    #[test]
    fn a_short_oid_is_seven_characters_and_never_panics_on_a_short_one() {
        check!(short_oid("4d02b9f0c1e4aa") == "4d02b9f");
        check!(short_oid("abc") == "abc");
        check!(short_oid("").is_empty());
    }

    #[test]
    fn a_future_timestamp_does_not_underflow() {
        // Commit timestamps are attacker-controlled: git takes whatever the
        // committer's clock said.
        let future = time::OffsetDateTime::now_utc().unix_timestamp() + 86_400;
        check!(relative_time(future) == "just now");
    }
}
