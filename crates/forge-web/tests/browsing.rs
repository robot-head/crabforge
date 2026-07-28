//! The forge through a browser: sign up, browse a repository, file an issue.
//!
//! These drive the real router with real HTML responses, so a template that
//! fails to render or a route that shadows another is caught here rather than
//! by a person clicking around.

use std::{collections::HashMap, sync::Arc};

use assert2::check;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use forge_command::CommandService;
use forge_git::{ObjectWriter, import};
use forge_projector::Projector;
use forge_store::Store;
use forge_testkit::{TestBroker, require_gres};
use forge_types::topics;
use forge_web::{WebState, router};
use tower::ServiceExt as _;

struct Site {
    _gres: forge_testkit::Gres,
    broker: TestBroker,
    cache_root: tempfile::TempDir,
    store: Arc<Store>,
    commands: Arc<CommandService>,
    /// One writer for the whole site. Object writes share a transactional
    /// identity, so a second one would fence this and every later write would
    /// fail — which is the fencing mechanism working, not a bug in it.
    object_writer: Arc<forge_bus::FencedWriter>,
    app: axum::Router,
    /// The session cookie, once signed in.
    cookie: Option<String>,
}

impl Site {
    async fn start() -> Option<Self> {
        let gres = require_gres().await?;
        let broker = TestBroker::with_forge_topics().await;
        let cache_root = tempfile::tempdir().unwrap();

        let store = Arc::new(Store::connect(&gres.dsn()).await.unwrap());
        store.migrate().await.unwrap();
        let commands = CommandService::start(&broker.bootstrap()).await.unwrap();

        let object_writer = Arc::new(
            forge_git::connect_object_writer(&broker.bootstrap())
                .await
                .unwrap(),
        );

        let mut applied = HashMap::new();
        for topic in [
            topics::EVENTS_USERS,
            topics::EVENTS_REPOS,
            topics::EVENTS_ISSUES,
            topics::EVENTS_PRS,
            topics::EVENTS_GIT_REFS,
        ] {
            let projector = Projector::open(
                &broker.bootstrap(),
                topic,
                Store::connect(&gres.dsn()).await.unwrap(),
            )
            .await
            .unwrap();
            applied.insert(topic.to_string(), projector.applied());
            let name = topic.to_string();
            tokio::spawn(async move {
                if let Err(e) = projector.run().await {
                    // Swallowing this hides exactly the failures these tests
                    // exist to catch.
                    eprintln!("PROJECTOR {name} DIED: {e}");
                }
            });
        }

        let state = Arc::new(WebState {
            store: Arc::clone(&store),
            commands: Some(Arc::clone(&commands)),
            bootstrap: broker.bootstrap(),
            cache_root: cache_root.path().to_path_buf(),
            csrf_secret: b"test-secret".to_vec(),
            secure_cookies: false,
            applied,
            object_writer: Some(Arc::clone(&object_writer)),
        });

        Some(Self {
            _gres: gres,
            broker,
            cache_root,
            store,
            commands,
            object_writer,
            app: router().with_state(state),
            cookie: None,
        })
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let mut request = Request::builder().uri(path);
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn post(&mut self, path: &str, form: &str) -> (StatusCode, Option<String>) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(form.to_string())).unwrap())
            .await
            .unwrap();

        let status = response.status();
        if let Some(set) = response.headers().get(header::SET_COOKIE)
            && let Ok(value) = set.to_str()
            && let Some((pair, _)) = value.split_once(';')
            && !pair.ends_with('=')
        {
            self.cookie = Some(pair.to_string());
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_string);
        (status, location)
    }

    /// The CSRF token embedded in a page's forms.
    async fn csrf(&self, path: &str) -> String {
        let (_, body) = self.get(path).await;
        let marker = r#"name="csrf" value=""#;
        let start = body.find(marker).expect("page should carry a csrf token") + marker.len();
        body[start..].split('"').next().unwrap().to_string()
    }

    async fn sign_up(&mut self, username: &str) {
        let csrf = self.csrf("/register").await;
        let form = format!(
            "csrf={csrf}&username={username}&email={username}%40example.com&password=correct-horse-battery"
        );
        let (status, _) = self.post("/register", &form).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "registration should redirect"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_visitor_can_register_and_lands_on_their_profile() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;

    let (status, body) = site.get("/octocat").await;
    check!(status == StatusCode::OK);
    check!(body.contains("octocat"));
    check!(
        body.contains("Sign out"),
        "the page should show them signed in"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_form_without_a_valid_token_is_refused() {
    // The CSRF defence, exercised end to end rather than only in a unit test.
    let Some(mut site) = Site::start().await else {
        return;
    };
    let (status, _) = site
        .post(
            "/register",
            "csrf=forged&username=someone&email=a%40b.com&password=correct-horse-battery",
        )
        .await;

    check!(status == StatusCode::BAD_REQUEST);
    let (_, body) = site.get("/someone").await;
    check!(
        !body.contains("Sign out"),
        "no account should have been created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signing_in_and_out_works() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;

    let csrf = site.csrf("/octocat").await;
    let (status, _) = site.post("/logout", &format!("csrf={csrf}")).await;
    check!(status == StatusCode::SEE_OTHER);
    site.cookie = None;

    let (_, body) = site.get("/octocat").await;
    check!(body.contains("Sign in"), "should be signed out now");

    let csrf = site.csrf("/login").await;
    let (status, _) = site
        .post(
            "/login",
            &format!("csrf={csrf}&username=octocat&password=correct-horse-battery"),
        )
        .await;
    check!(status == StatusCode::SEE_OTHER);
    let (_, body) = site.get("/octocat").await;
    check!(body.contains("Sign out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_password_is_refused_without_saying_which_part_was_wrong() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.cookie = None;

    let csrf = site.csrf("/login").await;
    let (status, _) = site
        .post(
            "/login",
            &format!("csrf={csrf}&username=octocat&password=wrong"),
        )
        .await;
    check!(
        status == StatusCode::OK,
        "the form is re-rendered, not redirected"
    );

    // The same message for an unknown user, so accounts cannot be enumerated.
    let csrf = site.csrf("/login").await;
    let (_, _) = site
        .post(
            "/login",
            &format!("csrf={csrf}&username=nobody&password=wrong"),
        )
        .await;
    let (_, body) = site.get("/login").await;
    check!(!body.contains("no such user"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_page_renders_an_error_rather_than_crashing() {
    let Some(site) = Site::start().await else {
        return;
    };
    let (status, body) = site.get("/nobody-at-all").await;
    check!(status == StatusCode::NOT_FOUND);
    check!(body.contains("404"), "the error page should render");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_paths_are_not_treated_as_usernames() {
    // `/login` must be the sign-in page, not the profile of a user named
    // "login". The router order and the reserved-name list have to agree.
    let Some(site) = Site::start().await else {
        return;
    };
    let (status, body) = site.get("/login").await;
    check!(status == StatusCode::OK);
    check!(body.contains("Sign in"));
}

impl Site {
    /// The id of a registered user, read back from the projection.
    async fn user_id(&self, username: &str) -> forge_types::UserId {
        self.store
            .users()
            .by_username_lower(username)
            .await
            .unwrap()
            .expect("user should be projected")
            .user_id
            .parse()
            .unwrap()
    }

    /// Create a repository and put a real commit in its topic.
    async fn seed_repo(&self, owner: &str, name: &str, files: &[(&str, &[u8])]) {
        let owner_id = self.user_id(owner).await;
        let repo = self
            .commands
            .create_repo(forge_command::CreateRepo {
                owner: owner_id,
                owner_name: forge_types::Username::parse(owner).unwrap(),
                name: name.into(),
                description: Some("a repository".into()),
                visibility: forge_types::Visibility::Public,
            })
            .await
            .unwrap();

        let mut admin = self.broker.admin().await;
        forge_topics::ensure_repo(&mut admin, repo.id)
            .await
            .unwrap();

        let source = tempfile::tempdir().unwrap();
        let head = import::make_test_repo(source.path(), files).unwrap();
        ObjectWriter::new(&self.object_writer, repo.id)
            .put_all(&import::read_all_objects(source.path()).unwrap())
            .await
            .unwrap();

        let cache = forge_git::Cache::new(self.cache_root.path(), repo.id);
        cache
            .hydrate(&self.broker.bootstrap(), "main")
            .await
            .unwrap();
        cache.set_ref("refs/heads/main", head).unwrap();
        cache.set_head("refs/heads/main").unwrap();

        // The page resolves the repository through gres, so wait for it.
        forge_testkit::eventually(
            "the repository to be projected",
            std::time::Duration::from_secs(10),
            || {
                let store = Arc::clone(&self.store);
                let key = format!("{owner}/{name}").to_ascii_lowercase();
                async move { store.repos().by_full_name(&key).await.unwrap().is_some() }
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repository_page_renders_its_tree_and_readme() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo(
        "octocat",
        "hello",
        &[
            ("README.md", b"# Hello\n\nThis is **rendered**.\n"),
            ("src/main.rs", b"fn main() { println!(\"hi\"); }\n"),
        ],
    )
    .await;

    let (status, body) = site.get("/octocat/hello").await;
    check!(status == StatusCode::OK, "got {status}");
    check!(body.contains("README.md"), "the tree should list the file");
    check!(body.contains("src"), "and the directory");
    check!(
        body.contains("<strong>rendered</strong>"),
        "the README should be rendered as markdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_renders_with_syntax_highlighting() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo(
        "octocat",
        "hello",
        &[("src/main.rs", b"fn main() { println!(\"hi\"); }\n")],
    )
    .await;

    let (status, body) = site.get("/octocat/hello/blob/main/src/main.rs").await;
    check!(status == StatusCode::OK, "got {status}");
    check!(body.contains("cf-"), "the file should be highlighted");
    check!(body.contains("main"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_file_is_never_served_as_html() {
    // A repository can contain anything. Serving it as markup on the forge's
    // own origin would let anyone with push access run script as the forge.
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo(
        "octocat",
        "hello",
        &[("evil.html", b"<script>alert(1)</script>\n")],
    )
    .await;

    let mut request = Request::builder().uri("/octocat/hello/raw/main/evil.html");
    if let Some(cookie) = &site.cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = site
        .app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    check!(content_type.starts_with("text/plain"), "got {content_type}");
    check!(
        response
            .headers()
            .get("x-content-type-options")
            .is_some_and(|v| v == "nosniff"),
        "a sniffing browser would ignore the content type without this"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_history_renders() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"x\n")])
        .await;

    let (status, body) = site.get("/octocat/hello/commits/main").await;
    check!(status == StatusCode::OK, "got {status}");
    check!(body.contains("initial commit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_issue_can_be_filed_and_read_back() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"x\n")])
        .await;

    let csrf = site.csrf("/octocat/hello/issues/new").await;
    let (status, location) = site
        .post(
            "/octocat/hello/issues",
            &format!("csrf={csrf}&title=Something+is+broken&body=It+**really**+is"),
        )
        .await;
    check!(status == StatusCode::SEE_OTHER, "got {status}");
    check!(location.as_deref() == Some("/octocat/hello/issues/1"));

    // The redirect target is readable immediately, because the handler waited
    // for its own projection before answering.
    let (status, body) = site.get("/octocat/hello/issues/1").await;
    check!(status == StatusCode::OK);
    check!(body.contains("Something is broken"));
    check!(
        body.contains("<strong>really</strong>"),
        "the body renders as markdown"
    );

    let (_, list) = site.get("/octocat/hello/issues").await;
    check!(list.contains("Something is broken"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_issue_can_be_commented_on_and_closed() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"x\n")])
        .await;

    let csrf = site.csrf("/octocat/hello/issues/new").await;
    site.post(
        "/octocat/hello/issues",
        &format!("csrf={csrf}&title=Discuss&body="),
    )
    .await;

    let csrf = site.csrf("/octocat/hello/issues/1").await;
    let (status, _) = site
        .post(
            "/octocat/hello/issues/1/comments",
            &format!("csrf={csrf}&body=Closing+this&action=close"),
        )
        .await;
    check!(status == StatusCode::SEE_OTHER);

    let (_, body) = site.get("/octocat/hello/issues/1").await;
    check!(body.contains("Closing this"), "the comment should appear");
    check!(body.contains("Closed"), "and the issue should be closed");
    check!(
        body.contains("Reopen issue"),
        "with the button now offering to reopen"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_out_visitor_can_read_but_not_write() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"x\n")])
        .await;
    let csrf = site.csrf("/octocat/hello/issues/new").await;
    site.post(
        "/octocat/hello/issues",
        &format!("csrf={csrf}&title=Public+issue&body="),
    )
    .await;

    site.cookie = None;

    let (status, body) = site.get("/octocat/hello").await;
    check!(
        status == StatusCode::OK,
        "a public repository is readable by anyone"
    );
    check!(body.contains("Sign in"));

    let (status, body) = site.get("/octocat/hello/issues/1").await;
    check!(status == StatusCode::OK);
    check!(body.contains("Public issue"));
    check!(
        !body.contains("Leave a comment"),
        "no composer when signed out"
    );

    // And the form endpoint itself refuses, not just the button being hidden.
    let (status, _) = site
        .post(
            "/octocat/hello/issues/1/comments",
            "csrf=whatever&body=sneaky&action=comment",
        )
        .await;
    check!(status == StatusCode::UNAUTHORIZED);
}

impl Site {
    /// Push a branch into an existing repository, through the command path.
    async fn add_branch(&self, owner: &str, name: &str, branch: &str, content: &[u8]) {
        let key = format!("{owner}/{name}").to_ascii_lowercase();
        let record = self
            .store
            .repos()
            .by_full_name(&key)
            .await
            .unwrap()
            .expect("repository should exist");
        let repo_id: forge_types::RepoId = record.repo_id.parse().unwrap();

        let source = tempfile::tempdir().unwrap();
        import::make_test_repo(source.path(), &[("f.txt", b"one\ntwo\nthree\n")]).unwrap();

        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(source.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["checkout", "-qb", branch]);
        std::fs::write(source.path().join("f.txt"), content).unwrap();
        run(&["commit", "-qam", "branch work"]);

        let main: forge_types::Oid = run(&["rev-parse", "main"]).parse().unwrap();
        let tip: forge_types::Oid = run(&["rev-parse", branch]).parse().unwrap();

        ObjectWriter::new(&self.object_writer, repo_id)
            .put_all(&import::read_all_objects(source.path()).unwrap())
            .await
            .unwrap();

        self.commands
            .update_refs(
                repo_id,
                vec![
                    forge_command::RefUpdate {
                        name: "refs/heads/main".into(),
                        expected_old: None,
                        new: Some(main),
                    },
                    forge_command::RefUpdate {
                        name: format!("refs/heads/{branch}"),
                        expected_old: None,
                        new: Some(tip),
                    },
                ],
                self.user_id(owner).await,
            )
            .await
            .unwrap();

        let cache = forge_git::Cache::new(self.cache_root.path(), repo_id);
        cache
            .hydrate(&self.broker.bootstrap(), "main")
            .await
            .unwrap();
        cache
            .sync_refs(&self.commands.refs_for(repo_id).await, "main")
            .unwrap();
    }

    /// Open a pull request and return its number.
    async fn open_pull(&self, owner: &str, name: &str, branch: &str) -> i64 {
        let key = format!("{owner}/{name}").to_ascii_lowercase();
        let record = self
            .store
            .repos()
            .by_full_name(&key)
            .await
            .unwrap()
            .unwrap();
        let repo_id: forge_types::RepoId = record.repo_id.parse().unwrap();
        let cache = forge_git::Cache::new(self.cache_root.path(), repo_id);

        let pr = self
            .commands
            .open_pull(forge_command::OpenPull {
                repo: repo_id,
                author: self.user_id(owner).await,
                author_name: owner.into(),
                title: "Merge my work".into(),
                body: Some("Please **review**.".into()),
                source_branch: branch.into(),
                target_branch: "main".into(),
                head_oid: cache.resolve(branch).unwrap().unwrap(),
                base_oid: cache.resolve("main").unwrap().unwrap(),
            })
            .await
            .unwrap();

        forge_testkit::eventually(
            "the pull request to be projected",
            std::time::Duration::from_secs(10),
            || {
                let store = Arc::clone(&self.store);
                let id = pr.id.to_string();
                async move { store.pulls().by_id(&id).await.unwrap().is_some() }
            },
        )
        .await;

        // Compute mergeability, as the worker would.
        let record = self
            .store
            .pulls()
            .by_id(&pr.id.to_string())
            .await
            .unwrap()
            .unwrap();
        forge_githttp::refresh_mergeability(&cache, &self.commands, repo_id, &record)
            .await
            .unwrap();
        forge_testkit::eventually(
            "mergeability to be recorded",
            std::time::Duration::from_secs(10),
            || {
                let store = Arc::clone(&self.store);
                let id = pr.id.to_string();
                async move {
                    store
                        .pulls()
                        .by_id(&id)
                        .await
                        .unwrap()
                        .unwrap()
                        .mergeability()
                        != forge_store::Mergeable::Unknown
                }
            },
        )
        .await;

        record.number
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pull_request_renders_with_its_diff_and_merge_button() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"one\ntwo\nthree\n")])
        .await;
    site.add_branch("octocat", "hello", "feature", b"ONE\ntwo\nthree\n")
        .await;
    let number = site.open_pull("octocat", "hello", "feature").await;

    let (status, body) = site.get(&format!("/octocat/hello/pulls/{number}")).await;
    check!(status == StatusCode::OK, "got {status}");
    check!(body.contains("Merge my work"));
    check!(
        body.contains("<strong>review</strong>"),
        "the body renders as markdown"
    );
    check!(body.contains("feature"), "and names the branches");
    check!(
        body.contains("Merge pull request"),
        "the button should be offered"
    );
    check!(body.contains("+ONE"), "the diff should show the change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merging_through_the_browser_moves_the_branch() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"one\ntwo\nthree\n")])
        .await;
    site.add_branch("octocat", "hello", "feature", b"ONE\ntwo\nthree\n")
        .await;
    let number = site.open_pull("octocat", "hello", "feature").await;

    let key = "octocat/hello";
    let record = site.store.repos().by_full_name(key).await.unwrap().unwrap();
    let repo_id: forge_types::RepoId = record.repo_id.parse().unwrap();
    let before = site
        .commands
        .refs_for(repo_id)
        .await
        .into_iter()
        .find(|(n, _)| n == "refs/heads/main")
        .unwrap()
        .1;

    let path = format!("/octocat/hello/pulls/{number}");
    let csrf = site.csrf(&path).await;
    let head = site
        .store
        .pulls()
        .by_number(&record.repo_id, number)
        .await
        .unwrap()
        .unwrap()
        .head_oid;

    let (status, _) = site
        .post(
            &format!("{path}/merge"),
            &format!("csrf={csrf}&expected_head={head}"),
        )
        .await;
    check!(status == StatusCode::SEE_OTHER, "got {status}");

    let after = site
        .commands
        .refs_for(repo_id)
        .await
        .into_iter()
        .find(|(n, _)| n == "refs/heads/main")
        .unwrap()
        .1;
    check!(after != before, "main should have advanced");

    forge_testkit::eventually(
        "the merge to be projected",
        std::time::Duration::from_secs(10),
        || {
            let store = Arc::clone(&site.store);
            let repo = record.repo_id.clone();
            async move {
                store
                    .pulls()
                    .by_number(&repo, number)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_merged()
            }
        },
    )
    .await;

    let (_, body) = site.get(&path).await;
    check!(body.contains("Merged as"), "the page should show it merged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merging_against_a_stale_head_is_refused() {
    // The reviewer approved one diff; if the branch moved, a different one
    // would land.
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"one\ntwo\nthree\n")])
        .await;
    site.add_branch("octocat", "hello", "feature", b"ONE\ntwo\nthree\n")
        .await;
    let number = site.open_pull("octocat", "hello", "feature").await;

    let path = format!("/octocat/hello/pulls/{number}");
    let csrf = site.csrf(&path).await;
    let (status, _) = site
        .post(
            &format!("{path}/merge"),
            &format!("csrf={csrf}&expected_head=0000000000000000000000000000000000000000"),
        )
        .await;

    check!(
        status == StatusCode::UNPROCESSABLE_ENTITY,
        "a merge from a stale page must be refused, got {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_review_appears_on_the_pull_request() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"one\ntwo\nthree\n")])
        .await;
    site.add_branch("octocat", "hello", "feature", b"ONE\ntwo\nthree\n")
        .await;
    let number = site.open_pull("octocat", "hello", "feature").await;

    let path = format!("/octocat/hello/pulls/{number}");
    let csrf = site.csrf(&path).await;
    let (status, _) = site
        .post(
            &format!("{path}/reviews"),
            &format!("csrf={csrf}&body=Looks+good&verdict=approve"),
        )
        .await;
    check!(status == StatusCode::SEE_OTHER);

    let (_, body) = site.get(&path).await;
    check!(body.contains("Approved"));
    check!(body.contains("Looks good"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_out_visitor_sees_no_merge_button() {
    let Some(mut site) = Site::start().await else {
        return;
    };
    site.sign_up("octocat").await;
    site.seed_repo("octocat", "hello", &[("f.txt", b"one\ntwo\nthree\n")])
        .await;
    site.add_branch("octocat", "hello", "feature", b"ONE\ntwo\nthree\n")
        .await;
    let number = site.open_pull("octocat", "hello", "feature").await;
    site.cookie = None;

    let (status, body) = site.get(&format!("/octocat/hello/pulls/{number}")).await;
    check!(
        status == StatusCode::OK,
        "the pull request is still readable"
    );
    check!(!body.contains("Merge pull request</button>"));
    check!(body.contains("can be merged"), "but it says so");
}
