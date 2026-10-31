//! M8 E12-2: append-only audit ledger (migration 079).
use std::sync::Arc;

use aionui_db::audit::{AUDIT_GENESIS_HASH, secret_fingerprint};
use aionui_db::{
    AuditActorKind, AuditEventFilter, AuditOrder, AuditSink, AuditVia, IAuditEventRepository, NewAuditEvent,
    SqliteAuditEventRepository, init_database, init_database_memory,
};
use serde_json::json;

fn event(action: &str, user: &str, ts: i64) -> NewAuditEvent {
    NewAuditEvent::new(action, AuditActorKind::User, AuditVia::Desktop)
        .with_user(user)
        .with_actor(user)
        .with_ts(ts)
        .with_summary(format!("{action} by {user}"))
}

#[tokio::test]
async fn append_chains_hashes_and_redacts_diff() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAuditEventRepository::new(db.pool().clone());

    let first = repo
        .append(
            &event("config.provider.update", "u1", 10)
                .with_target("provider", "p1")
                .with_diff(
                    json!({ "name": ["a", "b"], "api_key": ["sk-old-0000000000000000", "sk-new-0000000000000000"] }),
                )
                .with_approval("appr_1", Some("u1".into())),
        )
        .await
        .unwrap();
    assert_eq!(first.seq, 1);
    assert!(first.id.starts_with("aud_"));
    assert_eq!(first.prev_hash, AUDIT_GENESIS_HASH);
    assert_eq!(first.hash.len(), 64);
    assert_eq!(first.approval_id.as_deref(), Some("appr_1"));
    let diff = first.diff.as_deref().unwrap();
    assert!(!diff.contains("sk-old") && !diff.contains("sk-new"), "{diff}");
    assert!(diff.contains(&secret_fingerprint("sk-new-0000000000000000")));
    assert!(diff.contains("\"b\""));

    let second = repo.append(&event("approval.decide", "u1", 11)).await.unwrap();
    assert_eq!(second.seq, 2);
    assert_eq!(second.prev_hash, first.hash);

    let verification = repo.verify_chain().await.unwrap();
    assert!(verification.ok);
    assert_eq!(verification.checked, 2);
    assert_eq!(verification.head_hash.as_deref(), Some(second.hash.as_str()));
}

#[tokio::test]
async fn invalid_action_is_refused() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAuditEventRepository::new(db.pool().clone());
    assert!(repo.append(&event("Bad Action", "u1", 1)).await.is_err());
    assert!(repo.verify_chain().await.unwrap().head_hash.is_none());
}

#[tokio::test]
async fn update_and_delete_are_rejected_by_triggers() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAuditEventRepository::new(db.pool().clone());
    repo.append(&event("config.settings.update", "u1", 1)).await.unwrap();

    let update = sqlx::query("UPDATE audit_events SET summary = 'tampered'")
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(update.to_string().contains("append-only"), "{update}");
    let delete = sqlx::query("DELETE FROM audit_events")
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(delete.to_string().contains("append-only"), "{delete}");

    let rows = repo.list(&AuditEventFilter::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary, "config.settings.update by u1");
}

#[tokio::test]
async fn verify_detects_a_tampered_row() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAuditEventRepository::new(db.pool().clone());
    for i in 0..3 {
        repo.append(&event("config.settings.update", "u1", i)).await.unwrap();
    }
    // Someone with raw DB access drops the guard and edits row 2.
    sqlx::query("DROP TRIGGER audit_events_no_update")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET summary = 'tampered' WHERE seq = 2")
        .execute(db.pool())
        .await
        .unwrap();
    let verification = repo.verify_chain().await.unwrap();
    assert!(!verification.ok);
    assert_eq!(verification.checked, 1);
    assert_eq!(verification.first_broken_seq, Some(2));
}

#[tokio::test]
async fn list_filters_and_pages() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAuditEventRepository::new(db.pool().clone());
    repo.append(&event("config.provider.update", "u1", 100)).await.unwrap();
    repo.append(&event("config.mcp.create", "u1", 200)).await.unwrap();
    repo.append(&event("approval.decide", "u2", 300)).await.unwrap();
    repo.append(
        &NewAuditEvent::new("session_message.deliver", AuditActorKind::Agent, AuditVia::Helper)
            .with_user("u1")
            .with_actor("conv_1")
            .with_ts(400),
    )
    .await
    .unwrap();

    let mine = repo
        .list(&AuditEventFilter {
            user_id: Some("u1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        mine.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![4, 2, 1],
        "newest first, u1 only"
    );

    let config = repo
        .list(&AuditEventFilter {
            action_prefix: Some("config.".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(config.len(), 2);

    let exact = repo
        .list(&AuditEventFilter {
            action: Some("approval.decide".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(exact.len(), 1);

    let window = repo
        .list(&AuditEventFilter {
            from_ts: Some(150),
            to_ts: Some(300),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(window.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![3, 2]);

    let agent = repo
        .list(&AuditEventFilter {
            actor_kind: Some("agent".into()),
            actor_id: Some("conv_1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(agent.len(), 1);
    assert_eq!(agent[0].via, "helper");

    let page = repo
        .list(&AuditEventFilter {
            before_seq: Some(3),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2]);

    let asc = repo
        .list(&AuditEventFilter {
            after_seq: Some(1),
            order: AuditOrder::OldestFirst,
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(asc.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![2, 3]);
}

#[tokio::test]
async fn concurrent_appends_keep_a_single_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("audit.db")).await.unwrap();
    let repo: Arc<dyn IAuditEventRepository> = Arc::new(SqliteAuditEventRepository::new(db.pool().clone()));
    let sink = AuditSink::new(repo.clone());

    let mut tasks = Vec::new();
    for i in 0..24 {
        let sink = sink.clone();
        tasks.push(tokio::spawn(async move {
            sink.try_record(event("config.settings.update", "u1", i)).await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            ok += 1;
        }
    }
    assert_eq!(ok, 24, "every concurrent append lands");
    let verification = repo.verify_chain().await.unwrap();
    assert!(verification.ok, "{verification:?}");
    assert_eq!(verification.checked, 24);
}

#[tokio::test]
async fn sink_record_swallows_errors() {
    let db = init_database_memory().await.unwrap();
    let sink = AuditSink::new(Arc::new(SqliteAuditEventRepository::new(db.pool().clone())));
    assert!(sink.record(event("not valid", "u1", 1)).await.is_none());
    assert!(sink.record(event("config.export", "u1", 2)).await.is_some());
}
