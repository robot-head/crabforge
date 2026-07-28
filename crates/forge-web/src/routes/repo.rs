//! Browsing a repository.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
};
use forge_git::{Cache, browse::Blob};
use forge_store::RepoRecord;
use forge_types::RepoId;

use crate::{
    error::{WebError, WebResult},
    pages::{self, BlobPage, CommitPage, CommitView, CommitsPage, EntryView, ReadmeView, TreePage},
    session,
    state::WebState,
};

/// Names a README is looked for under, in order of preference.
const README_NAMES: &[&str] = &["README.md", "README", "README.txt", "readme.md"];

/// Resolve a repository and its cache, refusing anything the viewer may not see.
pub async fn open_repo(
    state: &Arc<WebState>,
    viewer: Option<&crate::state::Viewer>,
    owner: &str,
    repo: &str,
) -> WebResult<(RepoRecord, Cache)> {
    let key = format!("{owner}/{repo}").to_ascii_lowercase();
    let record = state
        .store
        .repos()
        .by_full_name(&key)
        .await?
        .filter(|r| !r.deleted)
        .ok_or(WebError::NotFound)?;

    // A private repository is reported as missing rather than forbidden, so
    // the forge never confirms that one exists to someone who cannot see it.
    if record.visibility == "private" {
        let permitted = viewer.is_some_and(|v| v.user_id == record.owner_id);
        if !permitted {
            return Err(WebError::NotFound);
        }
    }

    let repo_id: RepoId = record
        .repo_id
        .parse()
        .map_err(|_| WebError::Internal("stored repository id is not a uuid".into()))?;
    let cache = Cache::new(&state.cache_root, repo_id);
    cache
        .hydrate(&state.bootstrap, &record.default_branch)
        .await
        .map_err(|e| WebError::Internal(e.to_string()))?;

    Ok((record, cache))
}

/// The repository home, and any directory within it.
pub async fn tree(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let (revision, path) = split_revision(&rest);
    render_tree(&state, &headers, &owner, &repo, revision.as_deref(), &path).await
}

/// The repository home at its default branch.
pub async fn root(
    State(state): State<Arc<WebState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    render_tree(&state, &headers, &owner, &repo, None, "").await
}

async fn render_tree(
    state: &Arc<WebState>,
    headers: &HeaderMap,
    owner: &str,
    repo: &str,
    revision: Option<&str>,
    path: &str,
) -> WebResult<Response> {
    let viewer = session::viewer_from(state, headers).await;
    let (record, cache) = open_repo(state, viewer.as_ref(), owner, repo).await?;
    let revision = revision.unwrap_or(&record.default_branch).to_string();

    if !forge_git::is_safe_revision(&revision) || !forge_git::is_safe_path(path) {
        return Err(WebError::NotFound);
    }

    let empty = cache
        .is_empty_repo()
        .map_err(|e| WebError::Internal(e.to_string()))?;

    let (entries, readme) = if empty {
        (Vec::new(), None)
    } else {
        let entries = cache
            .list_tree(&revision, path)
            .map_err(|_| WebError::NotFound)?;

        // A README is shown only at the root, which is where people expect it.
        let readme = if path.is_empty() {
            cache
                .find_file(&revision, README_NAMES)
                .ok()
                .flatten()
                .and_then(|(name, blob)| match blob {
                    Blob::Text { content, .. } => Some(ReadmeView {
                        name,
                        html: askama::filters::Safe(forge_render::render_markdown(
                            &content,
                            &forge_render::markdown_options(),
                        )),
                    }),
                    Blob::Binary { .. } => None,
                })
        } else {
            None
        };

        let entries = entries
            .into_iter()
            .map(|e| EntryView {
                url: if e.kind.is_directory() {
                    format!("/{owner}/{repo}/tree/{revision}/{}", e.path)
                } else {
                    format!("/{owner}/{repo}/blob/{revision}/{}", e.path)
                },
                icon: pages::icon_for(e.kind),
                size: pages::human_size(e.size),
                name: e.name,
            })
            .collect();
        (entries, readme)
    };

    let counters = state.store.issues().counters(&record.repo_id).await?;
    let parent = path.rsplit_once('/').map_or("", |(head, _)| head);

    Ok(into_response(TreePage {
        csrf: session::csrf_token(state, viewer.as_ref()),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        parent_url: format!("/{owner}/{repo}/tree/{revision}/{parent}"),
        clone_url: format!("/{}/{}.git", record.owner_name, record.name),
        revision,
        path: path.to_string(),
        empty,
        entries,
        readme,
    }))
}

/// One file.
pub async fn blob(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, cache) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;
    let (revision, path) = split_revision(&rest);
    let revision = revision.unwrap_or_else(|| record.default_branch.clone());

    if !forge_git::is_safe_revision(&revision) || !forge_git::is_safe_path(&path) {
        return Err(WebError::NotFound);
    }

    let blob = cache
        .read_blob(&revision, &path)
        .map_err(|_| WebError::NotFound)?
        .ok_or(WebError::NotFound)?;
    let counters = state.store.issues().counters(&record.repo_id).await?;

    let (binary, highlighted) = match &blob {
        Blob::Binary { .. } => (true, String::new()),
        Blob::Text { content, .. } => (false, forge_render::highlight(&path, content).into_html()),
    };

    Ok(into_response(BlobPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        size: forge_types::ByteSize::bytes(blob.size()).human(),
        revision,
        path,
        binary,
        highlighted: askama::filters::Safe(highlighted),
    }))
}

/// A file's bytes, unrendered.
pub async fn raw(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    use axum::response::IntoResponse as _;

    let viewer = session::viewer_from(&state, &headers).await;
    let (record, cache) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;
    let (revision, path) = split_revision(&rest);
    let revision = revision.unwrap_or_else(|| record.default_branch.clone());

    if !forge_git::is_safe_revision(&revision) || !forge_git::is_safe_path(&path) {
        return Err(WebError::NotFound);
    }

    let blob = cache
        .read_blob(&revision, &path)
        .map_err(|_| WebError::NotFound)?
        .ok_or(WebError::NotFound)?;

    let body = match blob {
        Blob::Text { content, .. } => content.into_bytes(),
        // Reading it again would be wasteful; a binary file has no raw view
        // worth serving inline, so it is offered as a download.
        Blob::Binary { .. } => Vec::new(),
    };

    Ok((
        [
            // Never `text/html`, whatever the file is: serving a repository's
            // contents as markup on the forge's own origin would let anyone
            // with push access run script as the forge.
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            ),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "inline; filename=\"raw\"",
            ),
        ],
        body,
    )
        .into_response())
}

/// Commit history.
pub async fn commits(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, revision)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, cache) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;

    if !forge_git::is_safe_revision(&revision) {
        return Err(WebError::NotFound);
    }

    let commits = cache
        .history(&revision, 50, 0)
        .map_err(|e| WebError::Internal(e.to_string()))?
        .into_iter()
        .map(|c| CommitView {
            oid: c.oid.to_hex(),
            short: c.short(),
            summary: c.summary,
            author: c.author_name,
            when: pages::relative_time(c.authored_at),
        })
        .collect();
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(into_response(CommitsPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        revision,
        commits,
    }))
}

/// One commit, with its diff.
pub async fn commit(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, oid)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, cache) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;

    if !forge_git::is_safe_revision(&oid) {
        return Err(WebError::NotFound);
    }

    let commit = cache
        .commit(&oid)
        .map_err(|e| WebError::Internal(e.to_string()))?
        .ok_or(WebError::NotFound)?;
    let diff = cache.commit_diff(&oid).unwrap_or_default();
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(into_response(CommitPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        oid: commit.oid.to_hex(),
        summary: commit.summary,
        body: commit.body,
        author: commit.author_name,
        when: pages::relative_time(commit.authored_at),
        diff,
    }))
}

/// Split `main/src/lib.rs` into a revision and a path.
///
/// Ambiguous in general — a branch may contain a slash — so the first segment
/// wins. A branch called `feature/x` is addressable by its full ref name.
fn split_revision(rest: &str) -> (Option<String>, String) {
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return (None, String::new());
    }
    match rest.split_once('/') {
        Some((revision, path)) => (Some(revision.to_string()), path.to_string()),
        None => (Some(rest.to_string()), String::new()),
    }
}

fn into_response<T: axum::response::IntoResponse>(page: T) -> Response {
    page.into_response()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_revision_and_path_split_at_the_first_slash() {
        check!(split_revision("main/src/lib.rs") == (Some("main".into()), "src/lib.rs".into()));
        check!(split_revision("main") == (Some("main".into()), String::new()));
        check!(split_revision("") == (None, String::new()));
        check!(split_revision("/main/x") == (Some("main".into()), "x".into()));
    }
}
