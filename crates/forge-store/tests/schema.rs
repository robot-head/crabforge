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
    // Every migration the binary carries, whatever that is today — so adding
    // one does not require editing this test, and forgetting to register one
    // still fails.
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
    // A projector replaying the log re-applies events it has already seen.
    // Without ON CONFLICT this is a read-then-write, so it needs proving.
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
    let cursors = store.cursors();

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
        .cursors()
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
            .cursors()
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
            scopes: "repo:write".into(),
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
        scopes: "repo:read".into(),
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
