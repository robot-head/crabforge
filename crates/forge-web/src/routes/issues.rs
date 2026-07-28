//! Issues in the browser.

use std::sync::Arc;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use forge_command::{CommentOnIssue, OpenIssue};
use forge_types::{RepoId, topics};
use serde::Deserialize;

use crate::{
    error::{WebError, WebResult},
    pages::{CommentView, IssuePage, IssueRow, IssuesPage, NewIssuePage},
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
pub struct NewIssueForm {
    pub csrf: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Deserialize)]
pub struct CommentForm {
    pub csrf: String,
    #[serde(default)]
    pub body: String,
    /// `comment`, `close` or `reopen`.
    pub action: String,
}

/// Issues in a repository.
pub async fn list(
    State(state): State<Arc<WebState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, _) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;
    let showing_open = query.state.as_deref() != Some("closed");

    let issues = state
        .store
        .issues()
        .list(
            &record.repo_id,
            showing_open,
            None,
            forge_store::page_size(50),
        )
        .await?
        .into_iter()
        .map(|i| IssueRow {
            number: i.number,
            title: i.title,
            author: i.author_name,
            comments: i.comment_count,
        })
        .collect();
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(IssuesPage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        can_write: viewer.is_some(),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        closed_issues: counters.closed_issues,
        showing_open,
        issues,
    }
    .into_response())
}

/// The form for opening one.
pub async fn new_form(
    State(state): State<Arc<WebState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let Some(_) = viewer.as_ref() else {
        return Err(WebError::NeedsSignIn);
    };
    let (record, _) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(NewIssuePage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
    }
    .into_response())
}

/// Open an issue.
pub async fn create(
    State(state): State<Arc<WebState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<NewIssueForm>,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let viewer = viewer.ok_or(WebError::NeedsSignIn)?;
    if !session::is_genuine(&state, Some(&viewer), &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    let (record, _) = open_repo(&state, Some(&viewer), &owner, &repo).await?;
    let commands = state
        .commands
        .as_ref()
        .ok_or_else(|| WebError::Internal("the command service is not running".into()))?;
    let repo_id: RepoId = record
        .repo_id
        .parse()
        .map_err(|_| WebError::Internal("stored repository id is not a uuid".into()))?;
    let author = viewer
        .user_id
        .parse()
        .map_err(|_| WebError::Internal("stored user id is not a uuid".into()))?;

    let outcome = commands
        .open_issue(OpenIssue {
            repo: repo_id,
            author,
            author_name: viewer.username.clone(),
            title: form.title,
            body: Some(form.body),
        })
        .await
        .map_err(|e| WebError::BadRequest(e.to_string()))?;

    // Wait for the projection, so the redirect does not land on a 404 for an
    // issue that certainly exists.
    state
        .await_projection(
            topics::EVENTS_ISSUES,
            outcome.committed.offset_for(topics::EVENTS_ISSUES),
        )
        .await;

    let number = state
        .store
        .issues()
        .by_id(&outcome.id.to_string())
        .await?
        .map_or(1, |i| i.number);

    Ok(Redirect::to(&format!(
        "/{}/{}/issues/{number}",
        record.owner_name, record.name
    ))
    .into_response())
}

/// One issue and its conversation.
pub async fn detail(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let (record, _) = open_repo(&state, viewer.as_ref(), &owner, &repo).await?;

    let issue = state
        .store
        .issues()
        .by_number(&record.repo_id, number)
        .await?
        .ok_or(WebError::NotFound)?;

    let markdown = forge_render::markdown_options();
    let comments = state
        .store
        .issues()
        .comments(&issue.issue_id, forge_store::page_size(100))
        .await?
        .into_iter()
        .map(|c| CommentView {
            author: c.author_name,
            body: askama::filters::Safe(forge_render::render_markdown(&c.body, &markdown)),
        })
        .collect();
    let counters = state.store.issues().counters(&record.repo_id).await?;

    Ok(IssuePage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        can_write: viewer.is_some(),
        viewer,
        owner: record.owner_name.clone(),
        repo: record.name.clone(),
        description: record.description.clone(),
        private: record.visibility == "private",
        default_branch: record.default_branch.clone(),
        open_issues: counters.open_issues,
        number: issue.number,
        open: issue.is_open(),
        title: issue.title,
        author: issue.author_name,
        body: askama::filters::Safe(forge_render::render_markdown(
            issue.body.as_deref().unwrap_or_default(),
            &markdown,
        )),
        comments,
    }
    .into_response())
}

/// Comment on an issue, and optionally close or reopen it.
pub async fn comment(
    State(state): State<Arc<WebState>>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    Form(form): Form<CommentForm>,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let viewer = viewer.ok_or(WebError::NeedsSignIn)?;
    if !session::is_genuine(&state, Some(&viewer), &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    let (record, _) = open_repo(&state, Some(&viewer), &owner, &repo).await?;
    let issue = state
        .store
        .issues()
        .by_number(&record.repo_id, number)
        .await?
        .ok_or(WebError::NotFound)?;

    let commands = state
        .commands
        .as_ref()
        .ok_or_else(|| WebError::Internal("the command service is not running".into()))?;
    let repo_id: RepoId = record
        .repo_id
        .parse()
        .map_err(|_| WebError::Internal("stored repository id is not a uuid".into()))?;
    let issue_id = issue
        .issue_id
        .parse()
        .map_err(|_| WebError::Internal("stored issue id is not a uuid".into()))?;
    let author = viewer
        .user_id
        .parse()
        .map_err(|_| WebError::Internal("stored user id is not a uuid".into()))?;

    // A comment and a state change arrive from the same form. Post the comment
    // first when there is one, so closing with a parting remark records both in
    // the order they were meant.
    let mut last = None;
    if !form.body.trim().is_empty() {
        last = Some(
            commands
                .comment_on_issue(CommentOnIssue {
                    repo: repo_id,
                    issue: issue_id,
                    author,
                    author_name: viewer.username.clone(),
                    body: form.body,
                })
                .await
                .map_err(|e| WebError::BadRequest(e.to_string()))?
                .committed,
        );
    }

    match form.action.as_str() {
        "close" | "reopen" => {
            last = Some(
                commands
                    .set_issue_state(repo_id, issue_id, author, form.action == "reopen")
                    .await
                    .map_err(|e| WebError::BadRequest(e.to_string()))?
                    .committed,
            );
        }
        _ => {}
    }

    if let Some(committed) = last {
        state
            .await_projection(
                topics::EVENTS_ISSUES,
                committed.offset_for(topics::EVENTS_ISSUES),
            )
            .await;
    }

    Ok(Redirect::to(&format!(
        "/{}/{}/issues/{number}",
        record.owner_name, record.name
    ))
    .into_response())
}
