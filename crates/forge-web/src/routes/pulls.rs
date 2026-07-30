//! Pull requests in the browser.

use std::sync::Arc;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use forge_events::ReviewVerdict;
use forge_store::Mergeable;
use forge_types::{Oid, RepoId, topics};
use serde::Deserialize;

use crate::{
    error::{WebError, WebResult},
    pages::{CheckJobView, CheckView, PullDetailPage, PullRow, PullsPage, ReviewView},
    routes::repo::open_repo,
    session,
    state::WebState,
};

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Deserialize)]
pub struct ReviewForm {
    pub csrf: String,
    #[serde(default)]
    pub body: String,
    pub verdict: String,
}

#[derive(Deserialize)]
pub struct MergeForm {
    pub csrf: String,
    /// What the reviewer was looking at. A merge is refused if it has moved.
    pub expected_head: String,
}

pub async fn list(
    State(state): State<Arc<WebState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, _) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;
    let showing_open = query.state.as_deref() != Some("closed");

    let pulls = state
        .store
        .pulls()
        .list(&record.repo_id, showing_open, forge_store::page_size(50))
        .await?;
    let open_count = state
        .store
        .pulls()
        .list(&record.repo_id, true, forge_store::page_size(100))
        .await?
        .len() as i64;
    let closed_count = state
        .store
        .pulls()
        .list(&record.repo_id, false, forge_store::page_size(100))
        .await?
        .len() as i64;
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(PullsPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        tab: "pulls",
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        showing_open,
        open_count,
        closed_count,
        pulls: pulls
            .into_iter()
            .map(|p| PullRow {
                number: p.number,
                merged: p.is_merged(),
                title: p.title,
                author: p.author_name,
                source_branch: p.source_branch,
                target_branch: p.target_branch,
            })
            .collect(),
    }
    .into_response())
}

pub async fn detail(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, cache) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;

    let pr = state
        .store
        .pulls()
        .by_number(&record.repo_id, number)
        .await?
        .ok_or(WebError::NotFound)?;

    let markdown = forge_render::markdown_options();
    let reviews = state
        .store
        .pulls()
        .reviews(&pr.pr_id)
        .await?
        .into_iter()
        .map(|r| ReviewView {
            initials: crate::pages::initials(&r.reviewer_name),
            reviewer: r.reviewer_name,
            verdict_label: match r.verdict.as_str() {
                "approve" => "Approved",
                "request_changes" => "Changes requested",
                _ => "Commented",
            }
            .to_string(),
            verdict_class: r.verdict.clone(),
            body: r
                .body
                .map(|b| askama::filters::Safe(forge_render::render_markdown(&b, &markdown))),
        })
        .collect();

    // Empty unless the stored trial merge was run on the commits this request
    // currently points at — the record decides that, not this page.
    let conflicts = pr.conflicts().to_vec();
    let diff = cache
        .diff_between(&pr.base_oid, &pr.head_oid)
        .unwrap_or_default();
    let commit_count = cache
        .commits_between(&pr.base_oid, &pr.head_oid, 100)
        .map(|c| c.len() as i64)
        .unwrap_or(0);
    let counters = state.store.issues().counters(&record.repo_id).await?;

    // Crab Actions runs against the commit this request currently points at.
    // Keyed by commit rather than by pull request, so a run triggered by the
    // push shows here without CI needing to know what a pull request is.
    let runs = state
        .store
        .ci()
        .runs_for_commit(&pr.head_oid, forge_store::page_size(20))
        .await?;
    let mut checks = Vec::new();
    for run in &runs {
        let jobs = state.store.ci().jobs_of(&run.run_id).await?;
        checks.push(CheckView {
            name: run
                .workflow
                .rsplit('/')
                .next()
                .unwrap_or(&run.workflow)
                .to_string(),
            status_class: check_class(&run.status),
            glyph: check_glyph(&run.status),
            status: run.status.clone(),
            number: run.number,
            jobs: jobs
                .iter()
                .map(|job| CheckJobView {
                    name: job.name.clone(),
                    status_class: check_class(&job.status),
                    glyph: check_glyph(&job.status),
                    status: job.status.clone(),
                })
                .collect(),
        });
    }
    // Green only when everything has finished and everything passed. A run
    // still going is not a pass — showing one as green would put a merge button
    // in front of somebody before the tests had said anything.
    let checks_passed = runs.iter().all(|run| run.status == "success");

    let mergeability = pr.mergeability();
    Ok(PullDetailPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        can_write: viewer.is_some(),
        viewer,
        tab: "pulls",
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        number: pr.number,
        title: pr.title.clone(),
        state_label: if pr.is_merged() {
            "Merged"
        } else if pr.is_open() {
            "Open"
        } else {
            "Closed"
        }
        .to_string(),
        state_class: if pr.is_merged() {
            "merged"
        } else if pr.is_open() {
            "open"
        } else {
            "closed"
        }
        .to_string(),
        open: pr.is_open(),
        merged: pr.is_merged(),
        mergeable: mergeability == Mergeable::Clean,
        conflicted: mergeability == Mergeable::Conflict,
        conflicts,
        author_initials: crate::pages::initials(&pr.author_name),
        author: pr.author_name.clone(),
        body: pr
            .body
            .as_deref()
            .map(|b| askama::filters::Safe(forge_render::render_markdown(b, &markdown))),
        source_branch: pr.source_branch.clone(),
        target_branch: pr.target_branch.clone(),
        head_short: crate::pages::short_oid(&pr.head_oid),
        head_oid: pr.head_oid.clone(),
        merge_commit: pr.merge_commit_oid.clone().unwrap_or_default(),
        merged_by: pr.merged_by_name.clone(),
        commit_count,
        reviews,
        checks,
        checks_passed,
        diff,
    }
    .into_response())
}

pub async fn review(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    Form(form): Form<ReviewForm>,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let viewer = viewer.ok_or(WebError::NeedsSignIn)?;
    if !session::is_genuine(&state, Some(&viewer), &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    let (record, _) = open_repo(&state, Some(&viewer), &owner, &repo).await?;
    let pr = state
        .store
        .pulls()
        .by_number(&record.repo_id, number)
        .await?
        .ok_or(WebError::NotFound)?;
    let commands = state
        .commands
        .as_ref()
        .ok_or_else(|| WebError::Internal("the command service is not running".into()))?;

    let verdict = ReviewVerdict::parse(&form.verdict)
        .ok_or_else(|| WebError::BadRequest("unknown review verdict".into()))?;

    let outcome = commands
        .review_pull(forge_command::ReviewPull {
            repo: parse_repo(&record.repo_id)?,
            pr: pr
                .pr_id
                .parse()
                .map_err(|_| WebError::Internal("stored pull id is not a uuid".into()))?,
            reviewer: viewer
                .user_id
                .parse()
                .map_err(|_| WebError::Internal("stored user id is not a uuid".into()))?,
            reviewer_name: viewer.username.clone(),
            verdict,
            body: Some(form.body),
        })
        .await
        .map_err(|e| WebError::BadRequest(e.to_string()))?;

    state
        .await_projection(
            topics::EVENTS_PRS,
            outcome.committed.offset_for(topics::EVENTS_PRS),
        )
        .await;

    Ok(Redirect::to(&format!(
        "/{}/{}/pulls/{number}",
        record.owner_name, record.name
    ))
    .into_response())
}

/// Merge a pull request.
pub async fn merge(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    Form(form): Form<MergeForm>,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let viewer = viewer.ok_or(WebError::NeedsSignIn)?;
    if !session::is_genuine(&state, Some(&viewer), &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    let (record, cache) = open_repo(&state, Some(&viewer), &owner, &repo).await?;
    let pr = state
        .store
        .pulls()
        .by_number(&record.repo_id, number)
        .await?
        .ok_or(WebError::NotFound)?;

    // What the reviewer was looking at when they clicked. If the branch has
    // moved since the page rendered, the diff they approved is not the diff
    // that would land.
    if pr.head_oid != form.expected_head {
        return Err(WebError::BadRequest(
            "the branch moved while you were reviewing; reload and check the changes".into(),
        ));
    }

    let commands = state
        .commands
        .as_ref()
        .ok_or_else(|| WebError::Internal("the command service is not running".into()))?;
    let writer = state
        .object_writer
        .as_ref()
        .ok_or_else(|| WebError::Internal("no log writer is configured".into()))?;

    let merged = forge_githttp::perform_merge(
        &cache,
        writer,
        commands,
        parse_repo(&record.repo_id)?,
        &pr,
        &forge_githttp::Actor {
            id: viewer
                .user_id
                .parse()
                .map_err(|_| WebError::Internal("stored user id is not a uuid".into()))?,
            name: viewer.username.clone(),
            // A forge-local address: the merge commit needs one, and inventing
            // a real-looking address for someone would be worse.
            email: format!("{}@users.noreply.crabforge", viewer.username),
        },
    )
    .await
    .map_err(|e| match e {
        forge_githttp::MergeError::Conflicts(_) | forge_githttp::MergeError::Stale => {
            WebError::BadRequest(e.to_string())
        }
        other => WebError::Internal(other.to_string()),
    })?;

    state
        .await_projection(
            topics::EVENTS_PRS,
            merged.committed.offset_for(topics::EVENTS_PRS),
        )
        .await;

    Ok(Redirect::to(&format!(
        "/{}/{}/pulls/{number}",
        record.owner_name, record.name
    ))
    .into_response())
}

fn parse_repo(id: &str) -> WebResult<RepoId> {
    id.parse()
        .map_err(|_| WebError::Internal("stored repository id is not a uuid".into()))
}

/// Parse an object id from stored text.
#[allow(dead_code)]
fn parse_oid(text: &str) -> WebResult<Oid> {
    text.parse()
        .map_err(|_| WebError::Internal("stored object id is malformed".into()))
}

/// The CSS class for a run or job status.
///
/// Mapped here rather than in the template so an unrecognised status — one a
/// newer build writes — renders as neutral rather than as success.
fn check_class(status: &str) -> String {
    match status {
        "success" => "success",
        "failed" | "timed_out" => "failed",
        "infra_failed" => "infra",
        "running" => "running",
        "queued" => "queued",
        _ => "unknown",
    }
    .to_string()
}

/// The mark drawn beside a check's status.
///
/// A third channel after the word and the border colour, because the palette is
/// a mono one: pass and fail differ by two low-chroma tints that a reader with
/// a colour deficiency may not separate, and the shape is unambiguous.
fn check_glyph(status: &str) -> &'static str {
    match status {
        "success" => "✓",
        "failed" | "timed_out" => "✕",
        "infra_failed" => "!",
        "running" => "◐",
        _ => "○",
    }
}
