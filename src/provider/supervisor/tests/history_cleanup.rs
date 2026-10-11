//! History storage usage, batch cleanup and the cleanup policy sweep:
//! tenant scoping, active conversations, and the age and capacity rules.

use super::*;

async fn conversation(
    store: &ConversationStore,
    workspace: &Path,
    owner_id: &str,
    age_days: i64,
) -> ConversationManifest {
    let mut manifest =
        ConversationManifest::new(ProviderKind::Codex, workspace.to_path_buf(), None, None);
    manifest.owner_id = owner_id.to_owned();
    manifest.created_at = Utc::now() - chrono::Duration::days(age_days);
    manifest.updated_at = manifest.created_at;
    let created = store.create(manifest).await.unwrap();
    assert_eq!(
        (Utc::now() - created.updated_at).num_days(),
        age_days,
        "the fixture keeps the given updated_at"
    );
    created
}

fn conversation_dir(root: &Path, id: &str) -> PathBuf {
    let dir = root.join("data").join("conversations").join(id);
    assert!(dir.is_dir(), "{}", dir.display());
    dir
}

/// Adds `bytes` of padding to a conversation directory.
fn pad(root: &Path, id: &str, bytes: usize) {
    fs::write(
        conversation_dir(root, id).join("padding.bin"),
        vec![0u8; bytes],
    )
    .unwrap();
}

fn mark_active(supervisor: &ConversationSupervisor, id: &str) {
    let (cancel, _) = watch::channel(false);
    supervisor.active.insert(
        id.to_owned(),
        ActiveTurn {
            turn_id: "turn_test".to_owned(),
            cancel,
            provider: ProviderKind::Codex,
            provider_profile: None,
        },
    );
}

async fn bytes_of(supervisor: &ConversationSupervisor, owner_id: &str, id: &str) -> u64 {
    supervisor
        .storage_owned(owner_id)
        .await
        .unwrap()
        .into_iter()
        .find(|usage| usage.manifest.id == id)
        .map(|usage| usage.bytes)
        .unwrap()
}

#[tokio::test]
async fn storage_measures_owned_conversations_without_following_links() {
    let (root, store, supervisor, workspace) = control_fixture("todex-history-storage").await;
    let older = conversation(&store, &workspace, "local", 3).await;
    let newer = conversation(&store, &workspace, "local", 1).await;
    let foreign = conversation(&store, &workspace, "other", 2).await;
    let before = bytes_of(&supervisor, "local", &older.id).await;
    assert!(before > 0);
    pad(&root, &older.id, 5000);
    let nested = conversation_dir(&root, &older.id).join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("more.bin"), vec![1u8; 300]).unwrap();
    #[cfg(unix)]
    {
        let outside = root.join("outside.bin");
        fs::write(&outside, vec![0u8; 100_000]).unwrap();
        std::os::unix::fs::symlink(&outside, nested.join("link.bin")).unwrap();
    }

    let usage = supervisor.storage_owned("local").await.unwrap();
    let ids: Vec<&str> = usage
        .iter()
        .map(|usage| usage.manifest.id.as_str())
        .collect();
    assert_eq!(ids, vec![newer.id.as_str(), older.id.as_str()]);
    assert_eq!(usage[1].bytes, before + 5300);
    let other = supervisor.storage_owned("other").await.unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].manifest.id, foreign.id);

    // Serialized as the manifest plus `bytes`.
    let value = serde_json::to_value(&usage[1]).unwrap();
    assert_eq!(value["id"], older.id);
    assert_eq!(value["ownerId"], "local");
    assert_eq!(value["bytes"], before + 5300);
    assert!(value["updatedAt"].is_string());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn batch_cleanup_skips_active_missing_and_foreign_conversations() {
    let (root, store, supervisor, workspace) = control_fixture("todex-history-batch").await;
    let idle = conversation(&store, &workspace, "local", 1).await;
    let active = conversation(&store, &workspace, "local", 1).await;
    let foreign = conversation(&store, &workspace, "other", 1).await;
    pad(&root, &idle.id, 2048);
    let idle_bytes = bytes_of(&supervisor, "local", &idle.id).await;
    mark_active(&supervisor, &active.id);
    let missing = Uuid::new_v4().to_string();

    let report = supervisor
        .cleanup_owned(
            "local",
            &[
                idle.id.clone(),
                active.id.clone(),
                foreign.id.clone(),
                missing.clone(),
                idle.id.clone(),
            ],
        )
        .await;
    assert_eq!(report.deleted, vec![idle.id.clone()]);
    assert_eq!(report.freed_bytes, idle_bytes);
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|skip| (skip.conversation_id.as_str(), skip.code.as_str()))
        .collect();
    assert_eq!(
        skipped,
        vec![
            (active.id.as_str(), "CONFLICT"),
            (foreign.id.as_str(), "NOT_FOUND"),
            (missing.as_str(), "NOT_FOUND"),
        ]
    );
    assert!(report.skipped[0]
        .message
        .contains("active conversation cannot be deleted"));
    assert!(matches!(
        store.get(&idle.id).await,
        Err(AppError::NotFound(_))
    ));
    assert!(store.get(&active.id).await.is_ok());
    assert!(store.get(&foreign.id).await.is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn policy_age_rule_keeps_active_and_other_tenants() {
    let (root, store, supervisor, workspace) = control_fixture("todex-history-policy-age").await;
    let old = conversation(&store, &workspace, "local", 40).await;
    let old_active = conversation(&store, &workspace, "local", 40).await;
    let recent = conversation(&store, &workspace, "local", 5).await;
    let foreign_old = conversation(&store, &workspace, "other", 40).await;
    mark_active(&supervisor, &old_active.id);
    let old_bytes = bytes_of(&supervisor, "local", &old.id).await;

    let report = supervisor
        .apply_cleanup_policy(
            "local",
            CleanupThresholds {
                max_age_days: Some(30),
                max_total_bytes: None,
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![old.id.clone()]);
    assert!(report.skipped.is_empty());
    assert_eq!(report.freed_bytes, old_bytes);
    assert!(store.get(&old_active.id).await.is_ok());
    assert!(store.get(&recent.id).await.is_ok());
    assert!(store.get(&foreign_old.id).await.is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn policy_capacity_rule_removes_oldest_idle_first() {
    let (root, store, supervisor, workspace) =
        control_fixture("todex-history-policy-capacity").await;
    let oldest_active = conversation(&store, &workspace, "local", 30).await;
    let oldest_idle = conversation(&store, &workspace, "local", 20).await;
    let middle = conversation(&store, &workspace, "local", 10).await;
    let newest = conversation(&store, &workspace, "local", 1).await;
    let foreign = conversation(&store, &workspace, "other", 40).await;
    for manifest in [&oldest_active, &oldest_idle, &middle, &newest, &foreign] {
        pad(&root, &manifest.id, 100_000);
    }
    mark_active(&supervisor, &oldest_active.id);
    let total: u64 = supervisor
        .storage_owned("local")
        .await
        .unwrap()
        .iter()
        .map(|usage| usage.bytes)
        .sum();
    let newest_bytes = bytes_of(&supervisor, "local", &newest.id).await;

    // Room for all but about 1.5 conversations: the two oldest idle ones go.
    let report = supervisor
        .apply_cleanup_policy(
            "local",
            CleanupThresholds {
                max_age_days: None,
                max_total_bytes: Some(total - 150_000),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.deleted,
        vec![oldest_idle.id.clone(), middle.id.clone()]
    );
    assert!(store.get(&oldest_active.id).await.is_ok());
    assert!(store.get(&newest.id).await.is_ok());

    // A limit the active conversation alone exceeds still never removes it.
    let report = supervisor
        .apply_cleanup_policy(
            "local",
            CleanupThresholds {
                max_age_days: None,
                max_total_bytes: Some(1),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![newest.id.clone()]);
    assert_eq!(report.freed_bytes, newest_bytes);
    assert!(store.get(&oldest_active.id).await.is_ok());
    assert!(store.get(&foreign.id).await.is_ok());
    fs::remove_dir_all(root).unwrap();
}
