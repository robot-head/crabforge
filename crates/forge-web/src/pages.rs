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
    pub icon: &'static str,
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
    pub readme: Option<ReadmeView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/blob.html")]
pub struct BlobPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
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
    pub when: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "repo/commits.html")]
pub struct CommitsPage {
    pub viewer: MaybeViewer,
    pub csrf: String,
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
    pub body: Safe<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "issues/detail.html")]
pub struct IssuePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
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
    pub body: Safe<String>,
    pub comments: Vec<CommentView>,
    pub can_write: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "issues/new.html")]
pub struct NewIssuePage {
    pub viewer: MaybeViewer,
    pub csrf: String,
    pub owner: String,
    pub repo: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: String,
    pub open_issues: i64,
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

/// Render a byte count for a listing.
pub fn human_size(bytes: Option<u64>) -> String {
    bytes.map_or_else(String::new, |b| forge_types::ByteSize::bytes(b).human())
}

/// The icon for a tree entry.
pub fn icon_for(kind: forge_git::EntryKind) -> &'static str {
    use forge_git::EntryKind;
    match kind {
        EntryKind::Directory => "dir",
        EntryKind::Symlink => "link",
        EntryKind::Submodule => "sub",
        EntryKind::Executable => "exe",
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
    fn a_future_timestamp_does_not_underflow() {
        // Commit timestamps are attacker-controlled: git takes whatever the
        // committer's clock said.
        let future = time::OffsetDateTime::now_utc().unix_timestamp() + 86_400;
        check!(relative_time(future) == "just now");
    }
}
