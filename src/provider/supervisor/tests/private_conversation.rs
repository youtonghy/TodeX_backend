//! Private (RAM-only) conversations: nothing reaches the data directory,
//! refused operations, inherited privacy, private uploads and the notice
//! after a lost agent process.

use super::*;

/// Every path under `root` with its size (directories as `None`).
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<u64>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(read) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                pending.push(path.clone());
                entries.insert(path, None);
            } else {
                entries.insert(path, Some(metadata.len()));
            }
        }
    }
    entries
}

fn prompt(text: &str) -> ConversationPrompt {
    ConversationPrompt {
        client_request_id: None,
        text: text.to_owned(),
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        content: Vec::new(),
        permission_mode: None,
        work_mode: None,
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
    }
}

async fn history(store: &ConversationStore, id: &str) -> Vec<ConversationEvent> {
    store.complete_history(id).await.unwrap()
}

#[tokio::test]
async fn a_private_conversation_runs_without_touching_the_data_directory() {
    let (root, store, supervisor, workspace) = control_fixture("todex-private-runs").await;
    let data_dir = root.join("data");
    // Settle what the daemon creates on its own before the snapshot.
    supervisor.list_owned("local").await.unwrap();
    let before = tree(&data_dir);

    let manifest = supervisor
        .create_private_owned(
            "local",
            ProviderKind::Codex,
            workspace.clone(),
            Some("secret title".to_owned()),
            None,
        )
        .await
        .unwrap();
    assert!(manifest.ephemeral);
    assert_eq!(manifest.title.as_deref(), Some("secret title"));
    assert!(manifest.title_enc.is_none());
    assert_eq!(
        serde_json::to_value(&manifest).unwrap()["ephemeral"],
        json!(true)
    );
    supervisor
        .prompt_owned("local", &manifest.id, prompt("canary-private-prompt"))
        .await
        .unwrap();
    wait_until_idle(&supervisor).await;
    supervisor
        .update_metadata_owned(
            "local",
            &manifest.id,
            Some(Some("renamed".to_owned())),
            None,
        )
        .await
        .unwrap();

    let events = history(&store, &manifest.id).await;
    assert!(events
        .iter()
        .any(|event| event.event_type == "turn.completed"));
    // Plaintext: nothing was sealed.
    assert!(events
        .iter()
        .all(|event| crate::conversation::encrypted_content(&event.payload).is_none()));
    assert!(events
        .iter()
        .any(|event| event.payload.to_string().contains("canary-private-prompt")));
    let listed = supervisor.list_owned("local").await.unwrap();
    assert!(listed
        .iter()
        .any(|item| item.id == manifest.id && item.ephemeral));
    let replay = supervisor
        .replay_owned("local", &manifest.id, 0, 2, ReplayDetail::Summary)
        .await
        .unwrap();
    assert_eq!(replay.events.len(), 2);
    assert!(replay.has_more);
    let usage = supervisor.storage_owned("local").await.unwrap();
    assert_eq!(
        usage
            .iter()
            .find(|usage| usage.manifest.id == manifest.id)
            .unwrap()
            .bytes,
        0
    );
    // The Codex thread was started ephemeral, with memories off.
    let memory = fs::read_to_string(workspace.join("memory-requests.log")).unwrap();
    assert!(memory.contains("\"disabled\""), "{memory}");

    assert_eq!(tree(&data_dir), before, "the data directory changed");

    // A restart forgets it.
    let restarted = ConversationStore::new(data_dir.clone()).await.unwrap();
    assert!(restarted.list().await.unwrap().is_empty());

    supervisor
        .delete_owned("local", &manifest.id)
        .await
        .unwrap();
    assert!(store.get(&manifest.id).await.is_err());
    assert!(supervisor.list_owned("local").await.unwrap().is_empty());
    assert!(!crate::private_dirs::root()
        .conversation_path(&manifest.id)
        .exists());
    assert_eq!(tree(&data_dir), before);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn private_conversations_refuse_what_would_outlive_them() {
    let (root, _store, supervisor, workspace) = control_fixture("todex-private-refusals").await;
    for provider in [
        ProviderKind::GrokBuild,
        ProviderKind::Devin,
        ProviderKind::Acp,
        ProviderKind::Antigravity,
    ] {
        let error = supervisor
            .create_private_owned("local", provider, workspace.clone(), None, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "INVALID_REQUEST", "{provider:?}: {error}");
    }
    let manifest = supervisor
        .create_private_owned("local", ProviderKind::Codex, workspace.clone(), None, None)
        .await
        .unwrap();
    supervisor
        .prompt_owned("local", &manifest.id, prompt("first"))
        .await
        .unwrap();
    wait_until_idle(&supervisor).await;
    let refused = |label: &str, error: AppError| {
        assert_eq!(error.code(), "INVALID_REQUEST", "{label}: {error}");
        assert!(error.to_string().contains("private"), "{label}: {error}");
    };
    refused(
        "fork",
        supervisor
            .fork_owned("local", &manifest.id, None, None)
            .await
            .unwrap_err(),
    );
    refused(
        "fork at",
        supervisor
            .fork_owned("local", &manifest.id, None, Some("turn".to_owned()))
            .await
            .unwrap_err(),
    );
    refused(
        "compact",
        supervisor
            .compact_owned("local", &manifest.id, "compact-1", None)
            .await
            .unwrap_err(),
    );
    refused(
        "schedule",
        supervisor
            .schedule_owned(
                "local",
                &manifest.id,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .await
            .unwrap_err(),
    );
    // Retention and cleanup policies never pick it.
    assert!(supervisor
        .cleanup_expired(Utc::now() + chrono::Duration::days(1))
        .await
        .unwrap()
        .is_empty());
    let report = supervisor
        .apply_cleanup_policy(
            "local",
            CleanupThresholds {
                max_age_days: Some(0),
                max_total_bytes: Some(0),
            },
            Utc::now() + chrono::Duration::days(2),
        )
        .await
        .unwrap();
    assert!(report.deleted.is_empty());
    assert!(supervisor.get(&manifest.id).await.is_ok());

    // Children of a private conversation are private too.
    let child = supervisor
        .create_child_owned(
            "local",
            ProviderKind::Codex,
            workspace.clone(),
            None,
            None,
            &manifest.id,
        )
        .await
        .unwrap();
    assert!(child.ephemeral);
    let error = supervisor
        .create_child_owned(
            "local",
            ProviderKind::GrokBuild,
            workspace.clone(),
            None,
            None,
            &manifest.id,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "INVALID_REQUEST");
    supervisor.delete_owned("local", &child.id).await.unwrap();
    supervisor
        .delete_owned("local", &manifest.id)
        .await
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn private_uploads_are_prompt_files_of_their_conversation_only() {
    let (root, _store, supervisor, workspace) = control_fixture("todex-private-uploads").await;
    let manifest = supervisor
        .create_private_owned("local", ProviderKind::Codex, workspace.clone(), None, None)
        .await
        .unwrap();
    let private_root = crate::private_dirs::root();
    let uploads = private_root
        .ensure_subdir(&manifest.id, crate::private_dirs::UPLOADS_DIR)
        .unwrap();
    let stored = crate::prompt_uploads::write_chunk(&uploads, None, "notes.md", 0, b"# private")
        .await
        .unwrap();
    let file = || PromptContentRef::File {
        path: stored.path.clone(),
        name: None,
    };
    prepare_prompt_content(
        ProviderKind::Codex,
        &workspace,
        Some(manifest.id.as_str()),
        vec![file()],
    )
    .await
    .unwrap();
    // Another conversation (and a normal one) cannot name it.
    for other in [Some(uuid::Uuid::new_v4().to_string()), None] {
        let error = prepare_prompt_content(
            ProviderKind::Codex,
            &workspace,
            other.as_deref(),
            vec![file()],
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "INVALID_REQUEST");
    }
    supervisor
        .prompt_owned(
            "local",
            &manifest.id,
            ConversationPrompt {
                content: vec![file()],
                ..prompt("read the notes")
            },
        )
        .await
        .unwrap();
    wait_until_idle(&supervisor).await;
    supervisor
        .delete_owned("local", &manifest.id)
        .await
        .unwrap();
    assert!(!stored.path.exists());
    assert!(!private_root.conversation_path(&manifest.id).exists());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn a_lost_private_process_is_reported_and_starts_a_fresh_thread() {
    let (root, store, supervisor, workspace) = control_fixture("todex-private-lost").await;
    let manifest = supervisor
        .create_private_owned("local", ProviderKind::Codex, workspace.clone(), None, None)
        .await
        .unwrap();
    supervisor
        .prompt_owned("local", &manifest.id, prompt("first"))
        .await
        .unwrap();
    wait_until_idle(&supervisor).await;
    let lost = |events: &[ConversationEvent]| {
        events
            .iter()
            .filter(|event| {
                event.event_type == "provider.event"
                    && event.payload["providerMethod"] == "context_lost"
            })
            .count()
    };
    assert_eq!(lost(&history(&store, &manifest.id).await), 0);
    // The process goes away (a crash, a restart for new launch settings).
    supervisor
        .registry
        .driver(ProviderKind::Codex)
        .unwrap()
        .shutdown_session(&manifest.id)
        .await;
    supervisor
        .prompt_owned("local", &manifest.id, prompt("second"))
        .await
        .unwrap();
    wait_until_idle(&supervisor).await;
    let events = history(&store, &manifest.id).await;
    assert_eq!(lost(&events), 1);
    let notice = events
        .iter()
        .find(|event| event.payload["providerMethod"] == "context_lost")
        .unwrap();
    assert!(notice.payload["turnId"].is_string());
    assert_eq!(notice.payload["metadata"]["cause"], "process_restarted");
    // Never a resume: the fixture logs no resume and two memory requests.
    let memory = fs::read_to_string(workspace.join("memory-requests.log")).unwrap();
    assert_eq!(memory.lines().count(), 2);
    supervisor
        .delete_owned("local", &manifest.id)
        .await
        .unwrap();
    fs::remove_dir_all(root).unwrap();
}

/// Canary run of a real agent CLI, offline: `scripts`-free on purpose, it is
/// driven by environment variables (see `docs/API.md` "隐私对话" for what it
/// checks). Point the CLI at a mock model server and give it throwaway
/// HOME / config dirs; then grep them, the data directory and the private
/// folder root for the canary after this test deleted the conversation.
///
/// `TODEX_CANARY_PROVIDER` (claude-code | codex | pi | omp | opencode |
/// deepseek-harness), `TODEX_CANARY_BIN`, `TODEX_CANARY_ROOT` (data +
/// workspace go here), `TODEX_CANARY_TEXT`, optional `TODEX_CANARY_MODEL`,
/// `TODEX_CANARY_MODE`, `TODEX_CANARY_IMAGE` (a PNG sent with the prompt)
/// and `TODEX_CANARY_ENV` (comma-separated variables the CLI also gets, such
/// as a mock server's key).
#[tokio::test]
#[ignore = "drives a real agent CLI; run by hand with a mock model server"]
async fn canary_private_conversation_with_a_real_cli() {
    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let provider: ProviderKind = env("TODEX_CANARY_PROVIDER").parse().unwrap();
    let binary = env("TODEX_CANARY_BIN");
    let root = PathBuf::from(env("TODEX_CANARY_ROOT"));
    let text = env("TODEX_CANARY_TEXT");
    let workspace = root.join("workspaces").join("project");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let mut config = Config {
        data_dir: root.join("data"),
        workspace_roots: vec![fs::canonicalize(root.join("workspaces")).unwrap()],
        ..Config::default()
    };
    config.agent.claude_bin = binary.clone();
    config.agent.codex_bin = binary.clone();
    config.agent.omp_bin = binary.clone();
    config.agent.opencode_bin = binary.clone();
    config.agent.dsh_bin = binary.clone();
    config.agent.pi_bin = binary;
    let forwarded: Vec<String> = std::env::var("TODEX_CANARY_ENV")
        .unwrap_or_default()
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    for allowlist in [
        &mut config.agent.omp_env_allowlist,
        &mut config.agent.opencode_env_allowlist,
        &mut config.agent.dsh_env_allowlist,
    ] {
        allowlist.extend(forwarded.iter().cloned());
    }
    let config = Arc::new(config);
    let store = ConversationStore::new(config.data_dir.clone())
        .await
        .unwrap();
    let supervisor =
        ConversationSupervisor::new(config, store.clone(), ConversationEventHub::default());
    let manifest = supervisor
        .create_private_owned("local", provider, workspace, None, None)
        .await
        .unwrap();
    supervisor
        .prompt_owned(
            "local",
            &manifest.id,
            ConversationPrompt {
                model: std::env::var("TODEX_CANARY_MODEL").ok(),
                permission_mode: std::env::var("TODEX_CANARY_MODE").ok(),
                content: std::env::var("TODEX_CANARY_IMAGE")
                    .ok()
                    .map(|path| {
                        use base64::Engine;
                        let bytes = fs::read(path).unwrap();
                        vec![PromptContentRef::Image {
                            data: base64::engine::general_purpose::STANDARD.encode(bytes),
                            mime_type: "image/png".to_owned(),
                        }]
                    })
                    .unwrap_or_default(),
                ..prompt(&text)
            },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(240), async {
        while supervisor.has_active_turns() {
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the turn finishes");
    let events = history(&store, &manifest.id).await;
    for event in &events {
        let mut line = event.payload.to_string();
        line.truncate(300);
        println!("event {} {} {line}", event.sequence, event.event_type);
    }
    let canary = text
        .split_whitespace()
        .find(|word| word.starts_with("CANARY-"))
        .unwrap()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    let answered = events.iter().any(|event| {
        event.event_type != "message.created" && event.payload.to_string().contains(canary)
    });
    let folder = crate::private_dirs::root().conversation_path(&manifest.id);
    println!("private folder {}", folder.display());
    for (path, size) in tree(&folder) {
        println!("private file {} {size:?}", path.display());
    }
    supervisor
        .delete_owned("local", &manifest.id)
        .await
        .unwrap();
    sleep(Duration::from_secs(1)).await;
    assert!(!folder.exists());
    assert!(answered, "the agent's answer never repeated the canary");
}
