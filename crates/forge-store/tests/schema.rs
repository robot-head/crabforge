//! The schema, against a real gres instance.
//!
//! These are the tests that tell us whether our SQL actually runs on crabka's
//! Postgres engine rather than only on PostgreSQL proper. They skip when the
//! `crabka-gres` binary is not available — see `forge_testkit::require_gres`.

use assert2::{assert, check};
use forge_store::{RepoRecord, Store, StoreError, UserRecord, migrate};
use forge_testkit::require_gres;

async fn migrated_store() -> Option<(forge_testkit::Gres, Store)> {
    let gres = require_gres().await?;
    let store = Store::connect(&gres.dsn()).await.expect("connect to gres");
    store.migrate().await.expect("apply migrations");
    Some((gres, store))
}

fn sample_user(name: &str) -> UserRecord {
    let now = forge_types::now();
    UserRecord {
        user_id: forge_types::UserId::new().to_string(),
        username: name.to_string(),
        username_lower: name.to_ascii_lowercase(),
        email: format!("{name}@example.com"),
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_string(),
        display_name: None,
        bio: None,
        state: "active".to_string(),
        created_at: now,
        updated_at: now,
    }
}

fn sample_repo(owner: &UserRecord, name: &str) -> RepoRecord {
    let now = forge_types::now();
    RepoRecord {
        repo_id: forge_types::RepoId::new().to_string(),
        owner_id: owner.user_id.clone(),
        owner_name: owner.username.clone(),
        name: name.to_string(),
        full_name_lower: format!("{}/{}", owner.username_lower, name.to_ascii_lowercase()),
        description: Some("a repository".to_string()),
        default_branch: "main".to_string(),
        visibility: "public".to_string(),
        created_at: now,
        updated_at: now,
        deleted: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_apply_to_a_fresh_database() {
    let Some(gres) = require_gres().await else {
        return;
    };
    let store = Store::connect(&gres.dsn()).await.unwrap();

    let applied = store.migrate().await.expect("migrations must run on gres");
    // Every migration the binary carries, whatever that is today, so adding one
    // does not require editing this test. Note what this does *not* catch: both
    // sides come from MIGRATIONS, so an SQL file that was never registered
    // there is invisible here — the thing that catches that is the schema
    // failing to have the table.
    let expected: Vec<i64> = migrate::MIGRATIONS.iter().map(|m| m.version).collect();
    check!(applied == expected);
    check!(migrate::is_current(store.client()).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_are_idempotent() {
    // The dev loop runs them on every boot.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };

    let second = store.migrate().await.expect("re-running must be safe");
    check!(second.is_empty(), "nothing left to apply");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmigrated_database_is_reported_clearly() {
    let Some(gres) = require_gres().await else {
        return;
    };
    let store = Store::connect(&gres.dsn()).await.unwrap();

    let result = store.require_current_schema().await;
    assert!(let Err(StoreError::SchemaMismatch { found, expected }) = result);
    check!(found.is_none());
    check!(expected == migrate::expected_version());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn users_round_trip_through_gres() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let user = sample_user("Octocat");

    store.users().upsert(&user).await.expect("insert");
    let found = store.users().by_id(&user.user_id).await.unwrap();
    check!(found.as_ref() == Some(&user));

    // The indexed lookup the login path uses.
    let by_name = store.users().by_username_lower("octocat").await.unwrap();
    check!(by_name.as_ref() == Some(&user));
    check!(
        store
            .users()
            .by_username_lower("Octocat")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_replaces_rather_than_duplicating_on_replay() {
    // A projector replaying the log re-applies events it has already seen, so
    // the second apply has to land on the same row rather than beside it.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let mut user = sample_user("replayed");

    store.users().upsert(&user).await.unwrap();
    user.bio = Some("updated on replay".to_string());
    store.users().upsert(&user).await.unwrap();

    check!(store.users().count().await.unwrap() == 1);
    let found = store.users().by_id(&user.user_id).await.unwrap().unwrap();
    check!(found.bio.as_deref() == Some("updated on replay"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_creation_does_not_move_the_creation_time() {
    // `created_at` is excluded from every DO UPDATE. If it were not, replaying
    // the log would restamp every account and repository with the replay's
    // clock, and "joined" dates would silently become "last rebuilt" dates.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let mut user = sample_user("founder");
    let born = user.created_at;
    store.users().upsert(&user).await.unwrap();

    user.created_at = born + time::Duration::days(365);
    user.updated_at = user.created_at;
    store.users().upsert(&user).await.unwrap();

    let found = store.users().by_id(&user.user_id).await.unwrap().unwrap();
    check!(found.created_at == born, "the birthday moved");
    check!(found.updated_at != born, "but the modification time should");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_username_is_refused_by_the_database() {
    // The command service is the only writer and holds a replay-built index of
    // claimed names, so this should never happen. The constraint is here so
    // that if it ever does, it surfaces as a rejected write rather than as two
    // accounts answering to one name.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    store.users().upsert(&sample_user("octocat")).await.unwrap();

    // A different account id, the same name.
    let result = store.users().upsert(&sample_user("octocat")).await;
    assert!(let Err(StoreError::Sql(_)) = result, "a second claim was allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_repositories_cannot_share_a_full_name() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let owner = sample_user("octocat");
    store.users().upsert(&owner).await.unwrap();
    store
        .repos()
        .upsert(&sample_repo(&owner, "hello"))
        .await
        .unwrap();

    let result = store.repos().upsert(&sample_repo(&owner, "hello")).await;
    assert!(let Err(StoreError::Sql(_)) = result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_issue_number_is_unique_within_its_repository_and_not_beyond() {
    // `#7` has to mean one thing in a repository and something else in the next
    // one. A constraint that got either half wrong would be invisible until
    // either two issues collided or a second repository could not open one.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let issue = |repo: &str, id: &str, number: i64| {
        let now = forge_types::now();
        forge_store::IssueRecord {
            issue_id: id.to_string(),
            repo_id: repo.to_string(),
            number,
            title: "t".into(),
            body: None,
            author_id: "u".into(),
            author_name: "octocat".into(),
            state: "open".into(),
            comment_count: 0,
            created_at: now,
            updated_at: now,
            closed_at: None,
        }
    };
    store
        .issues()
        .upsert(&issue("repo-a", "i1", 7))
        .await
        .unwrap();

    // Same repository, same number, different issue: refused.
    let clash = store.issues().upsert(&issue("repo-a", "i2", 7)).await;
    assert!(let Err(StoreError::Sql(_)) = clash);

    // Same number in another repository: allowed, and this is the half a
    // too-broad constraint would break.
    store
        .issues()
        .upsert(&issue("repo-b", "i3", 7))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_is_per_topic_and_partition() {
    // The projector's cursor is keyed on both. If the key were topic alone,
    // adding a second partition would silently overwrite the first one's
    // progress and replay or skip events.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    store
        .cursors(forge_store::PROJECTOR)
        .set_applied_offset("forge.events.repos", 5)
        .await
        .unwrap();
    store
        .cursors(forge_store::PROJECTOR)
        .set_applied_offset("forge.events.issues", 9)
        .await
        .unwrap();

    check!(
        store
            .cursors(forge_store::PROJECTOR)
            .applied_offset("forge.events.repos")
            .await
            .unwrap()
            == 5
    );
    check!(
        store
            .cursors(forge_store::PROJECTOR)
            .applied_offset("forge.events.issues")
            .await
            .unwrap()
            == 9
    );

    // Re-recording the same topic advances rather than duplicating.
    store
        .cursors(forge_store::PROJECTOR)
        .set_applied_offset("forge.events.repos", 11)
        .await
        .unwrap();
    check!(
        store
            .cursors(forge_store::PROJECTOR)
            .applied_offset("forge.events.repos")
            .await
            .unwrap()
            == 11
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repos_resolve_by_their_pre_lowered_full_name() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let owner = sample_user("Octocat");
    store.users().upsert(&owner).await.unwrap();
    let repo = sample_repo(&owner, "Hello-World");
    store.repos().upsert(&repo).await.unwrap();

    let found = store
        .repos()
        .by_full_name("octocat/hello-world")
        .await
        .unwrap();
    check!(found.as_ref() == Some(&repo));
}

#[test]
fn a_page_size_cannot_carry_a_value_that_would_be_unsafe_in_sql() {
    // gres cannot bind a parameter in LIMIT, so the count is interpolated into
    // the statement text. The guarantee that this is safe is the type: every
    // value a `PageSize` can hold renders as a small positive integer.
    for candidate in [i64::MIN, -1, 0, forge_store::MAX_PAGE_SIZE + 1, i64::MAX] {
        check!(
            forge_store::PageSize::refine(candidate).is_err(),
            "{candidate} should not be constructible"
        );
    }
    for candidate in 1..=forge_store::MAX_PAGE_SIZE {
        let rendered = forge_store::PageSize::refine(candidate)
            .unwrap()
            .to_string();
        check!(rendered.chars().all(|c| c.is_ascii_digit()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_listing_is_keyset_paginated_newest_first() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let owner = sample_user("prolific");
    store.users().upsert(&owner).await.unwrap();

    let mut created = Vec::new();
    for i in 0..5 {
        let repo = sample_repo(&owner, &format!("repo-{i}"));
        store.repos().upsert(&repo).await.unwrap();
        created.push(repo.repo_id.clone());
    }

    let first = store
        .repos()
        .for_owner(&owner.user_id, None, forge_store::page_size(2))
        .await
        .unwrap();
    check!(first.len() == 2);
    // UUIDv7 is time-ordered, so descending id is newest-first.
    check!(first[0].repo_id == created[4]);
    check!(first[1].repo_id == created[3]);

    let next = store
        .repos()
        .for_owner(
            &owner.user_id,
            Some(&first[1].repo_id),
            forge_store::page_size(2),
        )
        .await
        .unwrap();
    check!(next.len() == 2);
    check!(
        next[0].repo_id == created[2],
        "cursor must not repeat a row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_repos_are_hidden_from_listings() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let owner = sample_user("tidy");
    store.users().upsert(&owner).await.unwrap();

    let mut repo = sample_repo(&owner, "temporary");
    store.repos().upsert(&repo).await.unwrap();
    repo.deleted = true;
    store.repos().upsert(&repo).await.unwrap();

    let listed = store
        .repos()
        .for_owner(&owner.user_id, None, forge_store::page_size(10))
        .await
        .unwrap();
    check!(listed.is_empty());
    // Still addressable by id, so history and audit views keep working.
    check!(store.repos().by_id(&repo.repo_id).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_projector_cursor_persists_and_advances() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let cursors = store.cursors(forge_store::PROJECTOR);

    // A topic never projected starts at the beginning of the log.
    check!(cursors.applied_offset("forge.events.repos").await.unwrap() == 0);

    cursors
        .set_applied_offset("forge.events.repos", 42)
        .await
        .unwrap();
    check!(cursors.applied_offset("forge.events.repos").await.unwrap() == 42);

    cursors
        .set_applied_offset("forge.events.repos", 99)
        .await
        .unwrap();
    check!(cursors.applied_offset("forge.events.repos").await.unwrap() == 99);

    // Cursors are per topic.
    check!(cursors.applied_offset("forge.events.users").await.unwrap() == 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transaction_rolls_back_rows_and_cursor_together() {
    // The exactly-once property: a batch's rows and the cursor that covers them
    // commit or roll back as one, so a crash mid-batch replays it cleanly.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let user = sample_user("atomic");

    store.client().batch_execute("BEGIN").await.unwrap();
    store.users().upsert(&user).await.unwrap();
    store
        .cursors(forge_store::PROJECTOR)
        .set_applied_offset("forge.events.users", 7)
        .await
        .unwrap();
    store.client().batch_execute("ROLLBACK").await.unwrap();

    check!(
        store.users().count().await.unwrap() == 0,
        "row must not survive"
    );
    check!(
        store
            .cursors(forge_store::PROJECTOR)
            .applied_offset("forge.events.users")
            .await
            .unwrap()
            == 0,
        "cursor must not survive either, or the batch would be skipped on replay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_and_tokens_round_trip_through_gres() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let auth = store.auth();
    let expires = forge_types::now() + time::Duration::days(14);

    auth.create_session("hash-a", "user-1", expires)
        .await
        .unwrap();
    let session = auth
        .session("hash-a")
        .await
        .unwrap()
        .expect("session should exist");
    check!(session.user_id == "user-1");

    auth.delete_session("hash-a").await.unwrap();
    check!(auth.session("hash-a").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_session_reads_as_absent() {
    // Returning it with a flag would mean every caller has to check, and one
    // that forgot would be an authentication bypass.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let past = forge_types::now() - time::Duration::hours(1);
    store
        .auth()
        .create_session("stale", "user-1", past)
        .await
        .unwrap();

    check!(store.auth().session("stale").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signing_out_everywhere_removes_every_session_for_that_user() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let auth = store.auth();
    let expires = forge_types::now() + time::Duration::days(1);
    for hash in ["laptop", "phone"] {
        auth.create_session(hash, "user-1", expires).await.unwrap();
    }
    auth.create_session("other-person", "user-2", expires)
        .await
        .unwrap();

    check!(auth.delete_sessions_for("user-1").await.unwrap() == 2);
    check!(auth.session("laptop").await.unwrap().is_none());
    check!(
        auth.session("other-person").await.unwrap().is_some(),
        "other users unaffected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_is_looked_up_by_the_hash_of_the_presented_secret() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let secret = forge_auth::mint_token().unwrap();
    let now = forge_types::now();
    store
        .auth()
        .upsert_token(&forge_store::AccessToken {
            token_id: "tok-1".into(),
            user_id: "user-1".into(),
            name: "laptop".into(),
            token_hash: forge_auth::digest(&secret),
            scopes: vec!["repo:write".into()],
            created_at: now,
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let found = store
        .auth()
        .token_by_hash(&forge_auth::digest(&secret))
        .await
        .unwrap()
        .expect("token should resolve");
    check!(found.user_id == "user-1");
    check!(found.is_usable(now));

    // The secret itself is nowhere in the database.
    check!(found.token_hash != secret);
    check!(store.auth().token_by_hash(&secret).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_token_stops_working_but_stays_listed() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let now = forge_types::now();
    let mut token = forge_store::AccessToken {
        token_id: "tok-1".into(),
        user_id: "user-1".into(),
        name: "ci".into(),
        token_hash: "h".into(),
        scopes: vec!["repo:read".into()],
        created_at: now,
        expires_at: None,
        revoked_at: None,
        last_used_at: None,
    };
    store.auth().upsert_token(&token).await.unwrap();

    token.revoked_at = Some(now);
    store.auth().upsert_token(&token).await.unwrap();

    let found = store.auth().token_by_hash("h").await.unwrap().unwrap();
    check!(!found.is_usable(now));
    // Still listed, so the settings page can show that it was revoked.
    check!(store.auth().tokens_for("user-1").await.unwrap().len() == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_keeps_its_last_use_across_a_replay() {
    // `last_used_at` is written directly by the web tier on every authenticated
    // request, and is deliberately absent from the DO UPDATE list. A projector
    // replaying the mint event must not roll it back to null — that would make
    // the settings page report an actively used token as never used.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let now = forge_types::now();
    let token = forge_store::AccessToken {
        token_id: "tok-used".into(),
        user_id: "user-1".into(),
        name: "laptop".into(),
        token_hash: "hh".into(),
        scopes: vec!["repo:write".into()],
        created_at: now,
        expires_at: None,
        revoked_at: None,
        last_used_at: None,
    };
    store.auth().upsert_token(&token).await.unwrap();
    store.auth().touch_token("tok-used").await.unwrap();

    // The projector re-applies the mint event, which still carries no use time.
    store.auth().upsert_token(&token).await.unwrap();

    let found = store.auth().token_by_hash("hh").await.unwrap().unwrap();
    check!(found.last_used_at.is_some(), "the last use was forgotten");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scopes_survive_the_array_column() {
    // Stored as text[], so the round trip has to preserve both the elements and
    // their order — an authorisation decision is made from what comes back.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let now = forge_types::now();
    store
        .auth()
        .upsert_token(&forge_store::AccessToken {
            token_id: "tok-scoped".into(),
            user_id: "user-1".into(),
            name: "ci".into(),
            token_hash: "scoped".into(),
            scopes: vec!["repo:read".into(), "repo:write".into(), "user".into()],
            created_at: now,
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let found = store.auth().token_by_hash("scoped").await.unwrap().unwrap();
    check!(found.scopes == ["repo:read", "repo:write", "user"]);

    // And they mean the same thing on the way out as on the way in.
    let scopes = forge_auth::Scopes::from_stored(&found.scopes);
    check!(scopes.allows(forge_auth::Scope::RepoWrite));
    check!(!scopes.allows(forge_auth::Scope::RepoAdmin));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_scope_array_is_not_the_same_as_a_missing_one() {
    // A token with no scopes grants nothing. Round-tripping it as NULL, or as
    // an array containing one empty string, would both be wrong in a way that
    // an authorisation check might read as permissive.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let now = forge_types::now();
    store
        .auth()
        .upsert_token(&forge_store::AccessToken {
            token_id: "tok-bare".into(),
            user_id: "user-1".into(),
            name: "bare".into(),
            token_hash: "bare".into(),
            scopes: Vec::new(),
            created_at: now,
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let found = store.auth().token_by_hash("bare").await.unwrap().unwrap();
    check!(found.scopes.is_empty());
    check!(forge_auth::Scopes::from_stored(&found.scopes).is_empty());
}

/// A pull request with no trial merge yet, for the tests below.
fn sample_pull(repo_id: &str, head: &str, base: &str) -> forge_store::PullRecord {
    let now = forge_types::now();
    forge_store::PullRecord {
        pr_id: forge_types::PrId::new().to_string(),
        repo_id: repo_id.to_string(),
        number: 1,
        title: "Add a thing".into(),
        body: None,
        author_id: "user-1".into(),
        author_name: "octocat".into(),
        state: "open".into(),
        source_branch: "feature".into(),
        target_branch: "main".into(),
        head_oid: head.into(),
        base_oid: base.into(),
        merge_check: None,
        merge_commit_oid: None,
        merged_by_name: None,
        comment_count: 0,
        created_at: now,
        updated_at: now,
        merged_at: None,
        closed_at: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_check_survives_the_jsonb_column() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let pr = sample_pull("repo-1", "aaa", "bbb");
    store.pulls().upsert(&pr).await.unwrap();

    // Nobody has looked yet.
    let found = store.pulls().by_id(&pr.pr_id).await.unwrap().unwrap();
    check!(found.merge_check.is_none());
    check!(found.mergeability() == forge_store::Mergeable::Unknown);

    let check = forge_store::MergeCheck::conflict(
        "aaa",
        "bbb",
        vec!["src/lib.rs".into(), "README.md".into()],
    );
    let applied = store
        .pulls()
        .record_check(&pr.pr_id, &check, forge_types::now())
        .await
        .unwrap();
    check!(applied);

    let found = store.pulls().by_id(&pr.pr_id).await.unwrap().unwrap();
    check!(found.merge_check.as_ref() == Some(&check));
    check!(found.mergeability() == forge_store::Mergeable::Conflict);
    check!(found.conflicts() == ["src/lib.rs", "README.md"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trial_merge_that_finishes_after_a_push_is_discarded() {
    // The race this schema exists to close: a trial merge takes long enough
    // that a push can land while it runs. Its answer is about the commit before
    // that push, and writing it would offer a merge button for a merge nobody
    // has tried.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let mut pr = sample_pull("repo-1", "old-head", "bbb");
    store.pulls().upsert(&pr).await.unwrap();

    // The push lands first.
    pr.head_oid = "new-head".into();
    store.pulls().upsert(&pr).await.unwrap();

    // Then the trial merge for the old head finishes.
    let stale = forge_store::MergeCheck::clean("old-head", "bbb");
    let applied = store
        .pulls()
        .record_check(&pr.pr_id, &stale, forge_types::now())
        .await
        .unwrap();
    check!(!applied, "a result for a commit that moved was stored");

    let found = store.pulls().by_id(&pr.pr_id).await.unwrap().unwrap();
    check!(found.merge_check.is_none());
    check!(!found.can_merge());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_after_a_clean_check_takes_the_merge_button_away() {
    // The same protection from the other direction: the check was current when
    // written, and stops counting the moment the branch moves — without anyone
    // having to remember to clear it.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let mut pr = sample_pull("repo-1", "head-1", "bbb");
    store.pulls().upsert(&pr).await.unwrap();
    store
        .pulls()
        .record_check(
            &pr.pr_id,
            &forge_store::MergeCheck::clean("head-1", "bbb"),
            forge_types::now(),
        )
        .await
        .unwrap();

    let found = store.pulls().by_id(&pr.pr_id).await.unwrap().unwrap();
    check!(found.can_merge());

    // A push moves the head. The stored check is untouched on purpose.
    pr.head_oid = "head-2".into();
    pr.merge_check = found.merge_check.clone();
    store.pulls().upsert(&pr).await.unwrap();

    let found = store.pulls().by_id(&pr.pr_id).await.unwrap().unwrap();
    check!(
        found.merge_check.is_some(),
        "the check should still be stored"
    );
    check!(
        found.mergeability() == forge_store::Mergeable::Unknown,
        "but it should no longer count"
    );
    check!(!found.can_merge());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_review_is_ignored_rather_than_doubled() {
    // Reviews are inserted with ON CONFLICT DO NOTHING: the id comes from the
    // event, so a redelivery is the same review, not a second one.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let review = forge_store::ReviewRecord {
        review_id: "rev-1".into(),
        pr_id: "pr-1".into(),
        repo_id: "repo-1".into(),
        reviewer_id: "user-2".into(),
        reviewer_name: "reviewer".into(),
        verdict: "approve".into(),
        body: Some("looks good".into()),
        created_at: forge_types::now(),
    };
    store.pulls().insert_review(&review).await.unwrap();
    store.pulls().insert_review(&review).await.unwrap();

    check!(store.pulls().reviews("pr-1").await.unwrap().len() == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_events_survive_the_array_column() {
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let now = forge_types::now();
    store
        .hooks()
        .upsert(&forge_store::WebhookRecord {
            webhook_id: "w-1".into(),
            repo_id: "repo-1".into(),
            url: "https://example.com/hook".into(),
            secret: "s3cret".into(),
            events: vec!["issue.*".into(), "git.ref_updated".into()],
            active: true,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let found = store.hooks().by_id("w-1").await.unwrap().unwrap();
    check!(found.events == ["issue.*", "git.ref_updated"]);
    // And a subscription still means what it meant before the round trip.
    check!(found.wants("issue.opened"));
    check!(found.wants("git.ref_updated"));
    check!(!found.wants("pr.opened"));
}

/// A queued job of `run_id`, for the CI tests below.
fn sample_job(run_id: &str, name: &str) -> forge_store::JobRecord {
    let now = forge_types::now();
    forge_store::JobRecord {
        job_id: forge_types::JobId::new().to_string(),
        run_id: run_id.to_string(),
        repo_id: "repo-1".into(),
        name: name.to_string(),
        image: "ubuntu:24.04".into(),
        status: "queued".into(),
        attempt: 0,
        exit_code: None,
        log_offset: None,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_one_runner_can_claim_a_job() {
    // The guarantee that makes at-least-once job delivery safe. Two runners can
    // be handed the same job — that is what at-least-once means — and exactly
    // one of them must get to run it. Without this, a push would build twice
    // and two runners would fight over one log.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let job = sample_job("run-1", "test");
    store.ci().upsert_job(&job).await.unwrap();

    let now = forge_types::now();
    let first = store
        .ci()
        .claim_job(&job.job_id, 1, 100, now)
        .await
        .unwrap();
    let second = store
        .ci()
        .claim_job(&job.job_id, 2, 200, now)
        .await
        .unwrap();

    check!(first, "the first runner should get the job");
    check!(!second, "the second runner must not");

    let stored = store.ci().job_by_id(&job.job_id).await.unwrap().unwrap();
    check!(stored.status == "running");
    check!(stored.attempt == 1, "the winner's attempt should stand");
    check!(stored.log_offset == Some(100), "and its log offset");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_job_is_not_restarted_by_a_redelivery() {
    // A rerun is a new job, not a second go at this one. Restarting a finished
    // job would blank a result someone may already have merged on.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let job = sample_job("run-1", "test");
    store.ci().upsert_job(&job).await.unwrap();
    let now = forge_types::now();
    store.ci().claim_job(&job.job_id, 1, 0, now).await.unwrap();
    store
        .ci()
        .finish_job(&job.job_id, 1, "success", Some(0), now)
        .await
        .unwrap();

    let reclaimed = store.ci().claim_job(&job.job_id, 2, 0, now).await.unwrap();
    check!(!reclaimed, "a finished job was restarted");

    let stored = store.ci().job_by_id(&job.job_id).await.unwrap().unwrap();
    check!(stored.status == "success");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zombie_runner_cannot_overwrite_the_live_attempt() {
    // The other half of at-least-once. A runner declared dead may still be
    // alive and about to report; its verdict is about a run nobody is waiting
    // for. Applying it would replace the live attempt's result — and since the
    // zombie is usually the one that was stuck, that means overwriting a real
    // answer with a stale one.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    let job = sample_job("run-1", "test");
    store.ci().upsert_job(&job).await.unwrap();
    let now = forge_types::now();

    // Attempt 1 claims it, then is presumed lost and the job is redelivered.
    store.ci().claim_job(&job.job_id, 1, 0, now).await.unwrap();
    store
        .ci()
        .finish_job(&job.job_id, 1, "failed", Some(1), now)
        .await
        .unwrap();

    // A second attempt takes over and succeeds. (Reset to queued as the
    // reconciler would, then claim.)
    let mut requeued = store.ci().job_by_id(&job.job_id).await.unwrap().unwrap();
    requeued.status = "queued".into();
    store.ci().upsert_job(&requeued).await.unwrap();
    check!(store.ci().claim_job(&job.job_id, 2, 0, now).await.unwrap());
    store
        .ci()
        .finish_job(&job.job_id, 2, "success", Some(0), now)
        .await
        .unwrap();

    // Now the zombie reports on attempt 1. It must not be heard.
    let applied = store
        .ci()
        .finish_job(&job.job_id, 1, "failed", Some(1), now)
        .await
        .unwrap();
    check!(!applied, "a stale attempt was allowed to report");

    let stored = store.ci().job_by_id(&job.job_id).await.unwrap().unwrap();
    check!(
        stored.status == "success",
        "the live result was overwritten"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_jobs_of_a_run_cannot_share_a_name() {
    // Job names come from a YAML map so they are unique by construction, and
    // the constraint says so — a duplicate would make "which job failed?"
    // ambiguous in every UI that shows one.
    let Some((_gres, store)) = migrated_store().await else {
        return;
    };
    store
        .ci()
        .upsert_job(&sample_job("run-1", "test"))
        .await
        .unwrap();

    let clash = store.ci().upsert_job(&sample_job("run-1", "test")).await;
    assert!(let Err(StoreError::Sql(_)) = clash);

    // The same name in a different run is fine, and is the common case.
    store
        .ci()
        .upsert_job(&sample_job("run-2", "test"))
        .await
        .unwrap();
}
