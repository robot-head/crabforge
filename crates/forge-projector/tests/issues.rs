//! Issues, from command to queryable row.

use std::sync::Arc;

use assert2::check;
use forge_command::{CommandService, CommentOnIssue, CreateRepo, OpenIssue, RegisterUser};
use forge_projector::Projector;
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::{RepoId, UserId, Username, Visibility, topics};

struct Harness {
    _gres: forge_testkit::Gres,
    dsn: String,
    broker: TestBroker,
    store: Arc<Store>,
    commands: Arc<CommandService>,
    repo: RepoId,
    author: UserId,
}

impl Harness {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
        store.migrate().await.unwrap();
        let commands = CommandService::start(&broker.bootstrap()).await.unwrap();

        let author = commands
            .register_user(RegisterUser {
                username: "octocat".into(),
                email: "octocat@example.com".into(),
                password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            })
            .await
            .unwrap();
        let repo = commands
            .create_repo(CreateRepo {
                owner: author.id,
                owner_name: Username::parse("octocat").unwrap(),
                name: "hello".into(),
                description: None,
                visibility: Visibility::Public,
            })
            .await
            .unwrap();

        Some(Self {
            dsn: gres.dsn(),
            _gres: gres,
            broker,
            store,
            commands,
            repo: repo.id,
            author: author.id,
        })
    }

    async fn project(&self) {
        Projector::open(
            &self.broker.bootstrap(),
            topics::EVENTS_ISSUES,
            Store::connect(&self.dsn).await.unwrap(),
        )
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    }

    async fn open_issue(&self, title: &str) -> forge_types::IssueId {
        self.commands
            .open_issue(OpenIssue {
                repo: self.repo,
                author: self.author,
                author_name: "octocat".into(),
                title: title.into(),
                body: Some(format!("Body of {title}")),
            })
            .await
            .unwrap()
            .id
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_opened_issue_becomes_a_queryable_row() {
    let Some(h) = Harness::start().await else {
        return;
    };
    h.open_issue("Something is broken").await;
    h.project().await;

    let issue = h
        .store
        .issues()
        .by_number(&h.repo.to_string(), 1)
        .await
        .unwrap()
        .expect("issue #1 should exist");
    check!(issue.title == "Something is broken");
    check!(issue.is_open());
    check!(issue.author_name == "octocat");
    check!(issue.comment_count == 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_numbers_are_sequential_per_repository() {
    // The number is what people cite. A gap or a duplicate is permanent.
    let Some(h) = Harness::start().await else {
        return;
    };
    for i in 1..=5 {
        h.open_issue(&format!("Issue {i}")).await;
    }
    h.project().await;

    for expected in 1..=5 {
        let issue = h
            .store
            .issues()
            .by_number(&h.repo.to_string(), expected)
            .await
            .unwrap();
        check!(issue.is_some(), "issue #{expected} is missing");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_numbers_survive_a_restart_without_reuse() {
    // The counter lives in the log, not in the process. Restarting must not
    // hand out #1 again and overwrite the first issue.
    let Some(h) = Harness::start().await else {
        return;
    };
    h.open_issue("Before the restart").await;

    let restarted = CommandService::start(&h.broker.bootstrap()).await.unwrap();
    restarted
        .open_issue(OpenIssue {
            repo: h.repo,
            author: h.author,
            author_name: "octocat".into(),
            title: "After the restart".into(),
            body: None,
        })
        .await
        .unwrap();
    h.project().await;

    let first = h
        .store
        .issues()
        .by_number(&h.repo.to_string(), 1)
        .await
        .unwrap();
    let second = h
        .store
        .issues()
        .by_number(&h.repo.to_string(), 2)
        .await
        .unwrap();
    check!(first.unwrap().title == "Before the restart");
    check!(
        second.unwrap().title == "After the restart",
        "the counter must not reset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn numbering_is_independent_between_repositories() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let other = h
        .commands
        .create_repo(CreateRepo {
            owner: h.author,
            owner_name: Username::parse("octocat").unwrap(),
            name: "other".into(),
            description: None,
            visibility: Visibility::Public,
        })
        .await
        .unwrap();

    h.open_issue("First here").await;
    h.commands
        .open_issue(OpenIssue {
            repo: other.id,
            author: h.author,
            author_name: "octocat".into(),
            title: "First there".into(),
            body: None,
        })
        .await
        .unwrap();
    h.project().await;

    // Both repositories have their own #1.
    check!(
        h.store
            .issues()
            .by_number(&h.repo.to_string(), 1)
            .await
            .unwrap()
            .unwrap()
            .title
            == "First here"
    );
    check!(
        h.store
            .issues()
            .by_number(&other.id.to_string(), 1)
            .await
            .unwrap()
            .unwrap()
            .title
            == "First there"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conversation_reads_back_in_order() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let issue = h.open_issue("Discuss").await;

    for body in ["first", "second", "third"] {
        h.commands
            .comment_on_issue(CommentOnIssue {
                repo: h.repo,
                issue,
                author: h.author,
                author_name: "octocat".into(),
                body: body.into(),
            })
            .await
            .unwrap();
    }
    h.project().await;

    let comments = h
        .store
        .issues()
        .comments(&issue.to_string(), forge_store::page_size(50))
        .await
        .unwrap();
    let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
    check!(bodies == vec!["first", "second", "third"]);

    let issue = h
        .store
        .issues()
        .by_id(&issue.to_string())
        .await
        .unwrap()
        .unwrap();
    check!(issue.comment_count == 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_and_reopening_moves_an_issue_between_lists() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let issue = h.open_issue("Temporary").await;
    h.project().await;
    check!(
        h.store
            .issues()
            .counters(&h.repo.to_string())
            .await
            .unwrap()
            .open_issues
            == 1
    );

    h.commands
        .set_issue_state(h.repo, issue, h.author, false)
        .await
        .unwrap();
    h.project().await;

    let closed = h
        .store
        .issues()
        .by_id(&issue.to_string())
        .await
        .unwrap()
        .unwrap();
    check!(!closed.is_open());
    check!(closed.closed_at.is_some());

    let counters = h
        .store
        .issues()
        .counters(&h.repo.to_string())
        .await
        .unwrap();
    check!(counters.open_issues == 0 && counters.closed_issues == 1);

    h.commands
        .set_issue_state(h.repo, issue, h.author, true)
        .await
        .unwrap();
    h.project().await;

    let reopened = h
        .store
        .issues()
        .by_id(&issue.to_string())
        .await
        .unwrap()
        .unwrap();
    check!(reopened.is_open());
    check!(
        reopened.closed_at.is_none(),
        "reopening clears the closing time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_is_filtered_by_state_and_paginated_newest_first() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let mut ids = Vec::new();
    for i in 1..=5 {
        ids.push(h.open_issue(&format!("Issue {i}")).await);
    }
    h.commands
        .set_issue_state(h.repo, ids[0], h.author, false)
        .await
        .unwrap();
    h.project().await;

    let open = h
        .store
        .issues()
        .list(&h.repo.to_string(), true, None, forge_store::page_size(2))
        .await
        .unwrap();
    check!(open.len() == 2);
    check!(open[0].number == 5, "newest first");
    check!(open[1].number == 4);

    let next = h
        .store
        .issues()
        .list(
            &h.repo.to_string(),
            true,
            Some(open[1].number),
            forge_store::page_size(2),
        )
        .await
        .unwrap();
    check!(next[0].number == 3, "the cursor must not repeat a row");

    let closed = h
        .store
        .issues()
        .list(&h.repo.to_string(), false, None, forge_store::page_size(10))
        .await
        .unwrap();
    check!(closed.len() == 1);
    check!(closed[0].number == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaying_the_whole_topic_leaves_the_same_state() {
    // The recovery property, for the issues projection specifically: counters
    // are recomputed rather than incremented, so a replay cannot double them.
    let Some(h) = Harness::start().await else {
        return;
    };
    let issue = h.open_issue("Replayed").await;
    h.commands
        .comment_on_issue(CommentOnIssue {
            repo: h.repo,
            issue,
            author: h.author,
            author_name: "octocat".into(),
            body: "a comment".into(),
        })
        .await
        .unwrap();
    h.project().await;

    let before = h
        .store
        .issues()
        .counters(&h.repo.to_string())
        .await
        .unwrap();

    h.store
        .cursors(forge_store::PROJECTOR)
        .set_applied_offset(topics::EVENTS_ISSUES, 0)
        .await
        .unwrap();
    h.project().await;

    let after = h
        .store
        .issues()
        .counters(&h.repo.to_string())
        .await
        .unwrap();
    check!(before == after, "counters drifted across a replay");

    let issue = h
        .store
        .issues()
        .by_id(&issue.to_string())
        .await
        .unwrap()
        .unwrap();
    check!(
        issue.comment_count == 1,
        "a redelivered comment must not be counted twice"
    );
    check!(
        h.store
            .issues()
            .comments(&issue.issue_id, forge_store::page_size(50))
            .await
            .unwrap()
            .len()
            == 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_and_oversized_text_is_refused() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let open = |title: String, body: Option<String>| {
        let commands = Arc::clone(&h.commands);
        let (repo, author) = (h.repo, h.author);
        async move {
            commands
                .open_issue(OpenIssue {
                    repo,
                    author,
                    author_name: "octocat".into(),
                    title,
                    body,
                })
                .await
        }
    };

    check!(
        open("   ".into(), None).await.is_err(),
        "a blank title is not a title"
    );
    check!(
        open("x".repeat(1000), None).await.is_err(),
        "titles are bounded"
    );
    check!(open("fine".into(), None).await.is_ok());
}
