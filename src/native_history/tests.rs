use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::conversation::{ConversationManifest, ProviderState};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

struct Fixture {
    root: PathBuf,
    state: AppState,
    roots: NativeRoots,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!("todex-native-history-{}", uuid::Uuid::new_v4()));
    let roots = NativeRoots {
        claude_dir: root.join("claude"),
        pi_session_dir: root.join("pi").join("sessions"),
        codex_home: root.join("codex"),
        grok_home: root.join("grok"),
        devin_dir: root.join("devin").join("cli"),
    };
    fs::create_dir_all(root.join("workspaces").join("project")).unwrap();
    let mut state = AppState::new_for_tests(Config {
        port: 0,
        data_dir: root.join("data"),
        workspace_roots: vec![root.join("workspaces")],
        ..Config::default()
    })
    .await
    .unwrap();
    Arc::make_mut(&mut state.config).security.enable_auth = false;
    state.native_history.roots_override = Some(roots.clone());
    // No real CLI is ever run: tests that delete install fake ones.
    let missing = root.join("bin").join("missing").display().to_string();
    state.native_history.clis = NativeClis {
        codex: missing.clone(),
        grok: missing.clone(),
        devin: missing,
        ..NativeClis::default()
    };
    Fixture { root, state, roots }
}

fn write_lines(path: &Path, records: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = records.iter().map(|record| format!("{record}\n")).collect();
    fs::write(path, text).unwrap();
}

/// Moves a file's mtime into the past.
fn age(path: &Path, by: Duration) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - by)
        .unwrap();
}

fn claude_session(fixture: &Fixture, project: &str, id: &str, prompt: &str) -> PathBuf {
    let path = fixture
        .roots
        .claude_dir
        .join("projects")
        .join(project)
        .join(format!("{id}.jsonl"));
    write_lines(
        &path,
        &[
            json!({"type": "user", "sessionId": id, "cwd": "/work/a", "timestamp": "2026-10-01T08:00:00Z",
                   "message": {"role": "user", "content": prompt}}),
            json!({"type": "assistant", "sessionId": id, "timestamp": "2026-10-01T08:00:05Z",
                   "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}}),
        ],
    );
    age(&path, DAY);
    path
}

fn pi_session(fixture: &Fixture, id: &str, prompt: &str) -> PathBuf {
    let path = fixture
        .roots
        .pi_session_dir
        .join("--work-b--")
        .join(format!("2026-10-01T08-00-00-000Z_{id}.jsonl"));
    write_lines(
        &path,
        &[
            json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-10-01T08:00:00.000Z", "cwd": "/work/b"}),
            json!({"type": "message", "id": "m1", "parentId": null, "timestamp": "2026-10-01T08:00:01.000Z",
                   "message": {"role": "user", "content": [{"type": "text", "text": prompt}], "timestamp": 1}}),
        ],
    );
    age(&path, DAY);
    path
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn list(fixture: &Fixture, provider: NativeProvider) -> Value {
    fixture
        .state
        .native_history
        .sessions(&fixture.state.conversations, provider, "local")
        .await
        .unwrap()
}

async fn reference(fixture: &Fixture, provider: ProviderKind, native_id: &str) -> String {
    let store = fixture.state.conversation_store();
    let manifest = store
        .create(ConversationManifest::new(
            provider,
            fixture.root.join("workspaces").join("project"),
            None,
            None,
        ))
        .await
        .unwrap();
    let mut provider_state = ProviderState::new(provider);
    provider_state.native_session_id = Some(native_id.to_owned());
    store
        .save_provider_state(&manifest.id, provider_state)
        .await
        .unwrap();
    manifest.id
}

#[test]
fn session_ids_reject_traversal() {
    for id in [
        "",
        "..",
        ".hidden",
        "a/b",
        "a\\b",
        "../etc",
        "a..b",
        "id\0",
        &"x".repeat(129),
    ] {
        assert!(validate_session_id(id).is_err(), "{id:?}");
    }
    assert!(validate_session_id(&new_id()).is_ok());
    assert!(validate_session_id("01a0c4b6-cdba-76ed-9382-ee5a0d731999").is_ok());
    assert!(validate_session_id("-rf").is_err());
    assert_eq!(
        NativeProvider::parse("codex").unwrap(),
        NativeProvider::Codex
    );
    assert_eq!(
        NativeProvider::parse("grok-build").unwrap(),
        NativeProvider::GrokBuild
    );
    assert!(NativeProvider::parse("opencode").is_err());
}

#[tokio::test]
async fn lists_both_formats_with_titles_and_side_data() {
    let fixture = fixture().await;
    let claude_dir = fixture.roots.claude_dir.clone();
    let old = new_id();
    let newer = new_id();
    let old_path = claude_session(&fixture, "-work-a", &old, "first prompt");
    age(&old_path, 3 * DAY);
    let newer_path = claude_session(&fixture, "-work-a", &newer, "second");
    // Side data keyed by the id, plus look-alikes that must not count.
    let sibling = newer_path.with_extension("");
    fs::create_dir_all(sibling.join("subagents")).unwrap();
    fs::write(
        sibling.join("subagents").join("agent-1.jsonl"),
        vec![b'x'; 100],
    )
    .unwrap();
    fs::create_dir_all(claude_dir.join("session-env").join(&newer)).unwrap();
    fs::write(
        claude_dir.join("session-env").join(&newer).join("env"),
        b"12345",
    )
    .unwrap();
    fs::create_dir_all(claude_dir.join("todos")).unwrap();
    fs::write(
        claude_dir
            .join("todos")
            .join(format!("{newer}-agent-{newer}.json")),
        b"[]",
    )
    .unwrap();
    fs::write(claude_dir.join("todos").join(format!("{old}x.json")), b"[]").unwrap();
    fs::create_dir_all(claude_dir.join("debug")).unwrap();
    fs::write(
        claude_dir.join("debug").join(format!("{newer}.txt")),
        b"dbg",
    )
    .unwrap();
    // Not sessions: a non-UUID file and a legacy agent sidechain file.
    let project = claude_dir.join("projects").join("-work-a");
    fs::write(project.join("notes.jsonl"), b"{}\n").unwrap();
    fs::write(project.join("agent-1234.jsonl"), b"{}\n").unwrap();

    let listing = list(&fixture, NativeProvider::ClaudeCode).await;
    assert_eq!(listing["count"], 2);
    let sessions = listing["sessions"].as_array().unwrap();
    assert_eq!(sessions[0]["id"], newer, "newest first");
    assert_eq!(sessions[1]["id"], old);
    assert_eq!(sessions[1]["title"], "first prompt");
    assert_eq!(sessions[1]["cwd"], "/work/a");
    assert_eq!(sessions[1]["createdAt"], "2026-10-01T08:00:00Z");
    assert_eq!(sessions[1]["running"], false);
    assert_eq!(sessions[1]["referencedBy"], json!([]));
    let transcript_bytes = fs::metadata(&newer_path).unwrap().len();
    assert_eq!(
        sessions[0]["bytes"],
        transcript_bytes + 100 + 5 + 2 + 3,
        "side data counted"
    );
    assert_eq!(
        listing["totalBytes"],
        sessions[0]["bytes"].as_u64().unwrap() + sessions[1]["bytes"].as_u64().unwrap()
    );
    assert_eq!(listing["root"], json!(claude_dir.join("projects")));

    // Pi: only files with a session header are sessions.
    let pi_id = "01a0c4b6-cdba-76ed-9382-ee5a0d731999";
    pi_session(&fixture, pi_id, "hello pi");
    let forwarding = fixture
        .roots
        .pi_session_dir
        .join("permission-forwarding")
        .join("serving");
    fs::create_dir_all(&forwarding).unwrap();
    fs::write(forwarding.join(format!("{pi_id}.json")), b"{\"x\":1}").unwrap();
    write_lines(
        &fixture
            .roots
            .pi_session_dir
            .join("--work-b--")
            .join("junk.jsonl"),
        &[json!({"type": "message"})],
    );
    let listing = list(&fixture, NativeProvider::Pi).await;
    assert_eq!(listing["count"], 1);
    assert_eq!(listing["sessions"][0]["id"], pi_id);
    assert_eq!(listing["sessions"][0]["title"], "hello pi");
    assert_eq!(listing["sessions"][0]["cwd"], "/work/b");

    let providers = fixture.state.native_history.providers().await.unwrap();
    assert_eq!(providers["providers"][0]["provider"], "claude-code");
    assert_eq!(providers["providers"][0]["available"], true);
    assert_eq!(providers["providers"][0]["sessionCount"], 2);
    assert_eq!(
        providers["providers"][0]["native"]["cleanupPeriodDays"],
        Value::Null
    );
    assert_eq!(providers["providers"][1]["provider"], "pi");
    assert_eq!(providers["providers"][1]["sessionCount"], 1);
    assert_eq!(providers["providers"][1]["native"], Value::Null);
}

#[tokio::test]
async fn cleanup_removes_side_data_and_protects_referenced_and_running_sessions() {
    let fixture = fixture().await;
    let native = &fixture.state.native_history;
    let claude_dir = fixture.roots.claude_dir.clone();
    let plain = new_id();
    let referenced = new_id();
    let running = new_id();
    let recent = new_id();
    let plain_path = claude_session(&fixture, "-work-a", &plain, "p");
    claude_session(&fixture, "-work-a", &referenced, "r");
    claude_session(&fixture, "-work-b", &running, "x");
    let recent_path = claude_session(&fixture, "-work-b", &recent, "y");
    age(&recent_path, Duration::ZERO);
    let side_dir = plain_path.with_extension("");
    fs::create_dir_all(side_dir.join("tool-results")).unwrap();
    fs::write(side_dir.join("tool-results").join("r.txt"), b"out").unwrap();
    fs::create_dir_all(claude_dir.join("session-env").join(&plain)).unwrap();
    fs::create_dir_all(claude_dir.join("todos")).unwrap();
    let todo = claude_dir
        .join("todos")
        .join(format!("{plain}-agent-{plain}.json"));
    fs::write(&todo, b"[]").unwrap();
    let other_todo = claude_dir
        .join("todos")
        .join(format!("{referenced}-agent.json"));
    fs::write(&other_todo, b"[]").unwrap();
    fs::write(claude_dir.join("settings.json"), b"{}").unwrap();
    fs::write(claude_dir.join("history.jsonl"), b"{}\n").unwrap();
    // A live marker (this test process) and a dead one.
    fs::create_dir_all(claude_dir.join("sessions")).unwrap();
    fs::write(
        claude_dir.join("sessions").join("1.json"),
        json!({"pid": std::process::id(), "sessionId": running}).to_string(),
    )
    .unwrap();
    fs::write(
        claude_dir.join("sessions").join("2.json"),
        json!({"pid": i32::MAX, "sessionId": plain}).to_string(),
    )
    .unwrap();
    let conversation = reference(&fixture, ProviderKind::ClaudeCode, &referenced).await;
    // A Pi conversation naming the same id does not protect a Claude session.
    reference(&fixture, ProviderKind::Pi, &plain).await;

    let listing = list(&fixture, NativeProvider::ClaudeCode).await;
    let by_id = |id: &str| {
        listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(&referenced)["referencedBy"], json!([conversation]));
    assert_eq!(by_id(&referenced)["running"], false);
    assert_eq!(by_id(&running)["running"], true);
    assert_eq!(by_id(&recent)["running"], true);
    assert_eq!(by_id(&plain)["running"], false);
    // Another tenant does not see the reference, but it still protects.
    let other = native
        .sessions(
            &fixture.state.conversations,
            NativeProvider::ClaudeCode,
            "other",
        )
        .await
        .unwrap();
    assert!(other["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["referencedBy"] == json!([])));

    let missing = new_id();
    let ids = vec![
        plain.clone(),
        referenced.clone(),
        running.clone(),
        recent.clone(),
        missing.clone(),
    ];
    let expected_freed = by_id(&plain)["bytes"].as_u64().unwrap();
    let report = native
        .cleanup(
            &fixture.state.conversations,
            NativeProvider::ClaudeCode,
            &ids,
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![plain.clone()]);
    assert_eq!(report.freed_bytes, expected_freed);
    let skipped: HashMap<&str, (&str, &str)> = report
        .skipped
        .iter()
        .map(|skip| {
            (
                skip.conversation_id.as_str(),
                (skip.code.as_str(), skip.message.as_str()),
            )
        })
        .collect();
    assert_eq!(skipped[referenced.as_str()].0, "CONFLICT");
    assert!(skipped[referenced.as_str()]
        .1
        .contains("TodeX conversation"));
    assert_eq!(skipped[running.as_str()].0, "CONFLICT");
    assert!(skipped[running.as_str()].1.contains("session is running"));
    assert_eq!(skipped[recent.as_str()].0, "CONFLICT");
    assert_eq!(skipped[missing.as_str()].0, "NOT_FOUND");

    assert!(!plain_path.exists());
    assert!(!side_dir.exists());
    assert!(!claude_dir.join("session-env").join(&plain).exists());
    assert!(!todo.exists());
    assert!(other_todo.exists());
    assert!(claude_dir.join("settings.json").exists());
    assert!(claude_dir.join("history.jsonl").exists());
    assert!(claude_dir.join("sessions").join("2.json").exists());
    assert_eq!(list(&fixture, NativeProvider::ClaudeCode).await["count"], 3);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_never_followed() {
    let fixture = fixture().await;
    let claude_dir = fixture.roots.claude_dir.clone();
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    // A symlinked project directory is not scanned.
    let linked = new_id();
    write_lines(
        &outside.join(format!("{linked}.jsonl")),
        &[json!({"type": "user", "message": {"role": "user", "content": "x"}})],
    );
    fs::create_dir_all(claude_dir.join("projects")).unwrap();
    std::os::unix::fs::symlink(&outside, claude_dir.join("projects").join("-linked")).unwrap();
    // A symlinked side-data entry is neither counted nor removed through.
    let id = new_id();
    let path = claude_session(&fixture, "-work-a", &id, "p");
    fs::create_dir_all(claude_dir.join("session-env")).unwrap();
    fs::write(outside.join("precious"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, claude_dir.join("session-env").join(&id)).unwrap();

    let listing = list(&fixture, NativeProvider::ClaudeCode).await;
    assert_eq!(listing["count"], 1);
    assert_eq!(
        listing["sessions"][0]["bytes"],
        fs::metadata(&path).unwrap().len()
    );
    let report = fixture
        .state
        .native_history
        .cleanup(
            &fixture.state.conversations,
            NativeProvider::ClaudeCode,
            &[id.clone(), linked.clone()],
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![id.clone()]);
    assert_eq!(report.skipped[0].code, "NOT_FOUND");
    assert!(!path.exists());
    assert!(outside.join("precious").exists());
    assert!(outside.join(format!("{linked}.jsonl")).exists());
    assert!(fs::symlink_metadata(claude_dir.join("session-env").join(&id)).is_ok());

    // Removal re-checks containment and refuses symlinks.
    let root = fs::canonicalize(claude_dir.join("projects")).unwrap();
    let error = remove_inside(&root, &outside.join("precious")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let error = remove_inside(&root, &claude_dir.join("projects").join("-linked")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(outside.join("precious").exists());
}

#[tokio::test]
async fn transcript_keeps_the_last_messages() {
    let fixture = fixture().await;
    let native = &fixture.state.native_history;
    let id = new_id();
    let path = fixture
        .roots
        .claude_dir
        .join("projects")
        .join("-work-a")
        .join(format!("{id}.jsonl"));
    let mut records = vec![json!({"type": "ai-title", "aiTitle": "Numbers"})];
    for index in 0..10 {
        records.push(
            json!({"type": "user", "cwd": "/w", "timestamp": "2026-10-01T08:00:00Z",
            "message": {"role": "user", "content": format!("q{index}")}}),
        );
        records.push(json!({"type": "assistant", "isSidechain": true,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hidden"}]}}));
    }
    write_lines(&path, &records);
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("not json\n");
    fs::write(&path, text).unwrap();

    let transcript = native
        .transcript(NativeProvider::ClaudeCode, &id, 3)
        .await
        .unwrap();
    assert_eq!(transcript["title"], "Numbers");
    assert_eq!(transcript["cwd"], "/w");
    assert_eq!(transcript["totalMessages"], 10);
    assert_eq!(transcript["truncated"], true);
    let texts: Vec<&str> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["q7", "q8", "q9"]);
    assert_eq!(transcript["messages"][0]["role"], "user");
    assert_eq!(transcript["messages"][0]["at"], "2026-10-01T08:00:00Z");
    let whole = native
        .transcript(NativeProvider::ClaudeCode, &id, 1000)
        .await
        .unwrap();
    assert_eq!(whole["truncated"], false);
    assert_eq!(whole["messages"].as_array().unwrap().len(), 10);

    assert!(matches!(
        native
            .transcript(NativeProvider::ClaudeCode, &new_id(), 10)
            .await,
        Err(AppError::NotFound(_))
    ));
    assert!(matches!(
        native
            .transcript(NativeProvider::ClaudeCode, "../x", 10)
            .await,
        Err(AppError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn policy_applies_age_then_capacity_and_keeps_protected_sessions() {
    let fixture = fixture().await;
    let native = &fixture.state.native_history;
    let ancient = new_id();
    let ancient_referenced = new_id();
    let middle = new_id();
    let newest = new_id();
    for (id, days) in [
        (&ancient, 40),
        (&ancient_referenced, 50),
        (&middle, 5),
        (&newest, 1),
    ] {
        let path = pi_session(&fixture, id, "work");
        // Pad so capacity matters.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{}\n",
            json!({"type": "custom", "pad": "x".repeat(1000)})
        ));
        fs::write(&path, text).unwrap();
        age(&path, DAY * days);
    }
    reference(&fixture, ProviderKind::Pi, &ancient_referenced).await;
    let listing = list(&fixture, NativeProvider::Pi).await;
    let bytes = |id: &str| {
        listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()["bytes"]
            .as_u64()
            .unwrap()
    };
    // Room for about two sessions: age removes `ancient`, then capacity
    // removes the oldest unprotected one (`middle`); the referenced session
    // stays although it is the oldest.
    let limit = bytes(&ancient_referenced) + bytes(&newest);
    let policy = CleanupPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_total_bytes: Some(limit),
    };
    native.cleanup.set_policy("pi", policy).await.unwrap();
    let (report, run) = run_policy(
        native,
        &fixture.state.conversations,
        NativeProvider::Pi,
        policy,
        CleanupTrigger::Manual,
    )
    .await
    .unwrap();
    assert_eq!(report.deleted, vec![ancient.clone(), middle.clone()]);
    assert!(report.skipped.is_empty());
    assert_eq!(report.freed_bytes, bytes(&ancient) + bytes(&middle));
    assert_eq!(run.deleted, 2);
    assert_eq!(run.trigger, CleanupTrigger::Manual);
    assert_eq!(native.cleanup.tenant("pi").await.last_run, Some(run));
    let remaining: Vec<String> = list(&fixture, NativeProvider::Pi).await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(remaining, vec![newest, ancient_referenced]);

    // The policy file is separate from the TodeX one and survives reloads.
    let data_dir = fixture.root.join("data");
    assert!(data_dir.join(NATIVE_HISTORY_CLEANUP_FILE).exists());
    let reloaded = NativeHistory::new(
        &data_dir,
        &HistoryCleanup::load(&data_dir),
        NativeClis::default(),
    );
    assert_eq!(reloaded.cleanup.tenant("pi").await.policy, policy);
    assert_eq!(
        fixture.state.history_cleanup.tenant("pi").await,
        Default::default()
    );
}

/// Installs an executable `/bin/sh` script as a fake provider CLI.
#[cfg(unix)]
fn fake_cli(fixture: &Fixture, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.root.join("bin").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn skips(report: &CleanupReport) -> HashMap<String, (String, String)> {
    report
        .skipped
        .iter()
        .map(|skip| {
            (
                skip.conversation_id.clone(),
                (skip.code.clone(), skip.message.clone()),
            )
        })
        .collect()
}

fn codex_rollout(fixture: &Fixture, dir: &str, id: &str, prompt: &str) -> PathBuf {
    let path = fixture
        .roots
        .codex_home
        .join(dir)
        .join(format!("rollout-2026-10-01T08-00-00-{id}.jsonl"));
    let item = |payload: Value| json!({"timestamp": "2026-10-01T08:00:01.000Z", "type": "response_item", "payload": payload});
    write_lines(
        &path,
        &[
            json!({"timestamp": "2026-10-01T08:00:00.000Z", "type": "session_meta",
                   "payload": {"id": id, "timestamp": "2026-10-01T08:00:00.000Z", "cwd": "/work/c", "source": "cli"}}),
            item(
                json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions>"}]}),
            ),
            item(
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>\n</environment_context>"}]}),
            ),
            item(
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /work/c"}]}),
            ),
            item(
                json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}]}),
            ),
            json!({"timestamp": "2026-10-01T08:00:01.000Z", "type": "event_msg", "payload": {"type": "user_message", "message": prompt}}),
            item(
                json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}]}),
            ),
            item(
                json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}", "call_id": "c1"}),
            ),
            item(json!({"type": "function_call_output", "call_id": "c1", "output": "a.txt"})),
            item(
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}),
            ),
        ],
    );
    age(&path, DAY);
    path
}

#[cfg(unix)]
#[tokio::test]
async fn codex_rollouts_are_listed_viewed_and_deleted_through_the_cli() {
    let mut fixture = fixture().await;
    let codex_home = fixture.roots.codex_home.clone();
    let [plain, archived, referenced, recent, failing] = [(); 5].map(|_| new_id());
    let plain_path = codex_rollout(&fixture, "sessions/2026/10/01", &plain, "fix the\n build");
    let archived_path = codex_rollout(&fixture, "archived_sessions", &archived, "old work");
    codex_rollout(&fixture, "sessions/2026/10/01", &referenced, "r");
    let recent_path = codex_rollout(&fixture, "sessions/2026/10/02", &recent, "y");
    age(&recent_path, Duration::ZERO);
    codex_rollout(&fixture, "sessions/2026/10/02", &failing, "f");
    // Not rollouts.
    fs::write(codex_home.join("sessions").join("notes.jsonl"), b"{}\n").unwrap();
    fs::write(
        codex_home.join(CODEX_SESSION_INDEX),
        format!(
            "{}\n{}\n{}\n",
            json!({"id": archived, "thread_name": "First name", "updated_at": "x"}),
            json!({"id": archived, "thread_name": "Named thread", "updated_at": "y"}),
            json!({"id": plain, "thread_name": "", "updated_at": "z"}),
        ),
    )
    .unwrap();
    let conversation = reference(&fixture, ProviderKind::Codex, &referenced).await;

    let listing = list(&fixture, NativeProvider::Codex).await;
    assert_eq!(listing["count"], 5);
    assert_eq!(listing["root"], json!(codex_home.join("sessions")));
    let by_id = |id: &str| {
        listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(&plain)["title"], "fix the build");
    assert_eq!(by_id(&plain)["cwd"], "/work/c");
    assert_eq!(by_id(&plain)["createdAt"], "2026-10-01T08:00:00Z");
    assert_eq!(
        by_id(&plain)["bytes"],
        fs::metadata(&plain_path).unwrap().len()
    );
    assert_eq!(by_id(&archived)["title"], "Named thread");
    assert_eq!(by_id(&referenced)["referencedBy"], json!([conversation]));
    assert_eq!(by_id(&recent)["running"], true);
    assert_eq!(by_id(&plain)["running"], false);

    let providers = fixture.state.native_history.providers().await.unwrap();
    let codex = &providers["providers"][2];
    assert_eq!(codex["provider"], "codex");
    assert_eq!(codex["available"], true);
    assert_eq!(codex["capabilities"], json!({"view": true, "sizes": true}));
    assert_eq!(codex["sessionCount"], 5);
    assert_eq!(codex["native"], Value::Null);

    let transcript = fixture
        .state
        .native_history
        .transcript(NativeProvider::Codex, &plain, 100)
        .await
        .unwrap();
    let messages: Vec<(String, String)> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap().to_owned(),
                message["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        messages,
        [
            ("user", "fix the\n build"),
            ("tool", r#"exec_command: {"cmd":"ls"}"#),
            ("tool", "a.txt"),
            ("assistant", "Done."),
        ]
        .map(|(role, text)| (role.to_owned(), text.to_owned()))
    );
    assert_eq!(transcript["messages"][0]["at"], "2026-10-01T08:00:01Z");

    // Deletion goes through `codex delete --force <id>` with CODEX_HOME set.
    let log = fixture.root.join("codex.log");
    let script = format!(
        "echo \"$CODEX_HOME|$*\" >> '{log}'\n\
         if [ \"$3\" = '{failing}' ]; then echo 'WARNING: proceeding' >&2; echo 'Error: failed to delete session' >&2; exit 1; fi\n\
         find \"$CODEX_HOME\" -name \"*$3.jsonl\" -delete\n\
         echo \"Deleted session $3.\"\n",
        log = log.display()
    );
    fixture.state.native_history.clis.codex = fake_cli(&fixture, "codex", &script);
    let expected_freed =
        by_id(&plain)["bytes"].as_u64().unwrap() + by_id(&archived)["bytes"].as_u64().unwrap();
    let missing = new_id();
    let report = fixture
        .state
        .native_history
        .cleanup(
            &fixture.state.conversations,
            NativeProvider::Codex,
            &[
                plain.clone(),
                archived.clone(),
                referenced.clone(),
                recent.clone(),
                failing.clone(),
                missing.clone(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![plain.clone(), archived.clone()]);
    assert_eq!(report.freed_bytes, expected_freed);
    let skipped = skips(&report);
    assert_eq!(skipped[&referenced].0, "CONFLICT");
    assert_eq!(skipped[&recent].0, "CONFLICT");
    assert_eq!(skipped[&missing].0, "NOT_FOUND");
    assert_eq!(skipped[&failing].0, "PROVIDER_UNAVAILABLE");
    assert!(skipped[&failing].1.ends_with("failed to delete session"));
    let home = codex_home.display();
    assert_eq!(
        log_lines(&log),
        [&plain, &archived, &failing].map(|id| format!("{home}|delete --force {id}"))
    );
    assert!(!plain_path.exists());
    assert!(!archived_path.exists());
    assert_eq!(list(&fixture, NativeProvider::Codex).await["count"], 3);
}

fn grok_session(fixture: &Fixture, id: &str, summary: Value, updates: &[Value]) -> PathBuf {
    let dir = fixture
        .roots
        .grok_home
        .join("sessions")
        .join("%2Fwork%2Fg")
        .join(id);
    fs::create_dir_all(&dir).unwrap();
    let mut summary = summary;
    summary["info"] = json!({"id": id, "cwd": "/work/g"});
    fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    write_lines(&dir.join("updates.jsonl"), updates);
    for name in ["summary.json", "updates.jsonl"] {
        age(&dir.join(name), DAY);
    }
    dir
}

fn grok_update(update: Value) -> Value {
    json!({"timestamp": 1790000000, "method": "session/update", "params": {"sessionId": "s", "update": update}})
}

#[cfg(unix)]
#[tokio::test]
async fn grok_sessions_are_listed_viewed_and_deleted_through_the_cli() {
    let mut fixture = fixture().await;
    let base = json!({"session_summary": "", "created_at": "2026-10-01T08:00:00Z",
                      "updated_at": "2026-10-01T09:00:00Z", "num_messages": 3, "current_model_id": "grok"});
    let [titled, summarized, gone, hidden] = [(); 4].map(|_| new_id());
    let mut summary = base.clone();
    summary["generated_title"] = json!("Fix the parser");
    summary["session_summary"] = json!("ignored");
    let titled_dir = grok_session(
        &fixture,
        &titled,
        summary,
        &[
            grok_update(
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "hello"}}),
            ),
            grok_update(
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
            ),
            grok_update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hi "}}),
            ),
            grok_update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "there"}}),
            ),
            grok_update(
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read file", "rawInput": {"path": "/a"}}),
            ),
            grok_update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}),
            ),
            grok_update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                               "content": [{"type": "content", "content": {"type": "text", "text": "file body"}}]}),
            ),
            grok_update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
            ),
            json!({"timestamp": 1790000001, "method": "_x.ai/session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "rewind_marker"}}}),
            grok_update(
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}),
            ),
        ],
    );
    let mut summary = base.clone();
    summary["session_summary"] = json!("Summarized  work");
    grok_session(&fixture, &summarized, summary, &[]);
    grok_session(&fixture, &gone, base.clone(), &[]);
    let mut summary = base.clone();
    summary["session_kind"] = json!("subagent");
    grok_session(&fixture, &hidden, summary, &[]);
    // A group's `.cwd` marker and a directory without summary are ignored.
    let group = fixture.roots.grok_home.join("sessions").join("%2Fwork%2Fg");
    fs::write(group.join(".cwd"), b"/work/g").unwrap();
    fs::create_dir_all(group.join(new_id())).unwrap();

    let listing = list(&fixture, NativeProvider::GrokBuild).await;
    assert_eq!(listing["count"], 3);
    let by_id = |id: &str| {
        listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(&titled)["title"], "Fix the parser");
    assert_eq!(by_id(&titled)["cwd"], "/work/g");
    assert_eq!(by_id(&titled)["createdAt"], "2026-10-01T08:00:00Z");
    assert_eq!(by_id(&titled)["bytes"], tree_bytes(&titled_dir));
    assert_eq!(by_id(&summarized)["title"], "Summarized work");
    assert_eq!(by_id(&gone)["title"], Value::Null);

    let transcript = fixture
        .state
        .native_history
        .transcript(NativeProvider::GrokBuild, &titled, 100)
        .await
        .unwrap();
    assert_eq!(transcript["title"], "Fix the parser");
    let texts: Vec<(&str, &str)> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap(),
                message["text"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        texts,
        [
            ("user", "hello"),
            ("assistant", "Hi there"),
            ("tool", r#"Read file: {"path":"/a"}"#),
            ("tool", "file body"),
            ("assistant", "Done."),
        ]
    );
    assert_eq!(transcript["messages"][0]["at"], "2026-09-21T14:13:20Z");

    // Without the CLI nothing is deleted.
    let native = &fixture.state.native_history;
    let report = native
        .cleanup(
            &fixture.state.conversations,
            NativeProvider::GrokBuild,
            std::slice::from_ref(&titled),
        )
        .await
        .unwrap();
    assert_eq!(report.skipped[0].code, "UNSUPPORTED");
    assert!(titled_dir.exists());

    let log = fixture.root.join("grok.log");
    let script = format!(
        "echo \"$GROK_HOME|$GROK_DISABLE_AUTOUPDATER|$*\" >> '{log}'\n\
         if [ \"$4\" = '{gone}' ]; then echo \"No session found with id $4.\"; exit 0; fi\n\
         find \"$GROK_HOME/sessions\" -mindepth 2 -maxdepth 2 -name \"$4\" -exec rm -rf {{}} +\n\
         echo \"Deleted session $4\"\n",
        log = log.display()
    );
    let grok = fake_cli(&fixture, "grok", &script);
    fixture.state.native_history.clis.grok = grok;
    let native = &fixture.state.native_history;
    let report = native
        .cleanup(
            &fixture.state.conversations,
            NativeProvider::GrokBuild,
            &[titled.clone(), gone.clone(), hidden.clone()],
        )
        .await
        .unwrap();
    assert_eq!(report.deleted, vec![titled.clone()]);
    assert_eq!(
        report.freed_bytes,
        by_id(&titled)["bytes"].as_u64().unwrap()
    );
    let skipped = skips(&report);
    assert_eq!(skipped[&gone].0, "NOT_FOUND");
    assert_eq!(skipped[&hidden].0, "NOT_FOUND");
    let home = fixture.roots.grok_home.display();
    assert_eq!(
        log_lines(&log),
        [&titled, &gone].map(|id| format!("{home}|1|--no-auto-update sessions delete {id}"))
    );
    assert!(!titled_dir.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn devin_sessions_are_listed_and_deleted_through_the_cli() {
    let mut fixture = fixture().await;
    let devin_dir = fixture.roots.devin_dir.clone();
    fs::create_dir_all(&devin_dir).unwrap();
    fs::write(devin_dir.join("sessions.db"), vec![0; 4096]).unwrap();
    fs::write(devin_dir.join("sessions.db-wal"), vec![0; 100]).unwrap();
    let [old, locked, vanished, recent] = [(); 4].map(|_| new_id());
    let entry = |id: &str, updated: &str, locked: bool, title: Option<&str>| {
        let mut entry = json!({"sessionId": id, "cwd": "/work/d", "updatedAt": updated,
            "_meta": {"cognition.ai/createdAt": "2026-08-01T09:00:00.000Z", "cognition.ai/isLocked": locked}});
        if let Some(title) = title {
            entry["title"] = json!(title);
        }
        entry
    };
    let now = Utc::now().to_rfc3339();
    let page1 = json!({"jsonrpc": "2.0", "id": 1, "result": {
        "sessions": [entry(&old, "2026-08-01T10:00:00+00:00", false, Some("Devin  one")),
                     entry(&locked, "2026-08-01T10:00:00+00:00", true, None)],
        "nextCursor": "p2"}});
    let page2 = json!({"jsonrpc": "2.0", "id": 2, "result": {
        "sessions": [entry(&vanished, "2026-08-02T10:00:00+00:00", false, None),
                     entry(&recent, &now, false, None),
                     {"sessionId": "../bad"}]}});
    let initialize = json!({"jsonrpc": "2.0", "id": 0, "result": {"protocolVersion": 1,
        "agentCapabilities": {"sessionCapabilities": {"list": {}, "delete": {}}}}});
    let log = fixture.root.join("devin.log");
    let script = format!(
        "echo \"$*\" >> '{log}'\n\
         if [ \"$1\" = acp ]; then\n\
           while IFS= read -r line; do\n\
             case \"$line\" in\n\
               *'\"method\":\"initialize\"'*) echo '{initialize}' ;;\n\
               *'\"cursor\":\"p2\"'*) echo '{page2}' ;;\n\
               *'\"method\":\"session/list\"'*) echo 'banner'; echo '{{\"jsonrpc\":\"2.0\",\"method\":\"note\"}}'; echo '{page1}' ;;\n\
             esac\n\
           done\n\
           exit 0\n\
         fi\n\
         if [ \"$3\" = '{vanished}' ]; then echo \"Error: no session found matching '$3'\" >&2; exit 1; fi\n\
         echo \"Session $3 deleted.\"\n",
        log = log.display()
    );
    fixture.state.native_history.clis.devin = fake_cli(&fixture, "devin", &script);
    let native = &fixture.state.native_history;

    let listing = list(&fixture, NativeProvider::Devin).await;
    assert_eq!(listing["count"], 4);
    assert_eq!(listing["totalBytes"], 4196);
    assert_eq!(listing["root"], json!(devin_dir));
    let by_id = |id: &str| {
        listing["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(&old)["title"], "Devin one");
    assert_eq!(by_id(&old)["cwd"], "/work/d");
    assert_eq!(by_id(&old)["bytes"], Value::Null);
    assert_eq!(by_id(&old)["createdAt"], "2026-08-01T09:00:00Z");
    assert_eq!(by_id(&old)["updatedAt"], "2026-08-01T10:00:00Z");
    assert_eq!(by_id(&old)["running"], false);
    assert_eq!(by_id(&locked)["running"], true);
    assert_eq!(by_id(&recent)["running"], true);

    let providers = native.providers().await.unwrap();
    let devin = &providers["providers"][4];
    assert_eq!(devin["provider"], "devin");
    assert_eq!(devin["available"], true);
    assert_eq!(
        devin["capabilities"],
        json!({"view": false, "sizes": false})
    );
    assert_eq!(devin["totalBytes"], 4196);
    assert!(matches!(
        native.transcript(NativeProvider::Devin, &old, 10).await,
        Err(AppError::Unsupported(_))
    ));

    // Sizes are unknown: a capacity rule is refused, the age rule works.
    let capacity = CleanupPolicy {
        enabled: true,
        max_age_days: None,
        max_total_bytes: Some(1),
    };
    assert!(matches!(
        NativeProvider::Devin.validate_policy(&capacity),
        Err(AppError::InvalidRequest(_))
    ));
    assert!(NativeProvider::Codex.validate_policy(&capacity).is_ok());
    let policy = CleanupPolicy {
        enabled: true,
        max_age_days: Some(30),
        max_total_bytes: None,
    };
    let (report, _) = run_policy(
        native,
        &fixture.state.conversations,
        NativeProvider::Devin,
        policy,
        CleanupTrigger::Manual,
    )
    .await
    .unwrap();
    assert_eq!(report.deleted, vec![old.clone()]);
    assert_eq!(report.freed_bytes, 0);
    let skipped = skips(&report);
    assert_eq!(skipped[&vanished].0, "NOT_FOUND");
    assert!(skipped[&vanished].1.contains("no session found matching"));
    assert!(!skipped.contains_key(&locked));
    let rm: Vec<String> = log_lines(&log)
        .into_iter()
        .filter(|line| line.starts_with("rm "))
        .collect();
    assert_eq!(rm, [&old, &vanished].map(|id| format!("rm --force {id}")));

    // Without the CLI the store is reported, but nothing is listed.
    fixture.state.native_history.clis.devin = fixture.root.join("none").display().to_string();
    let listing = list(&fixture, NativeProvider::Devin).await;
    assert_eq!(listing["count"], 0);
    let providers = fixture.state.native_history.providers().await.unwrap();
    assert_eq!(providers["providers"][4]["available"], false);
    assert_eq!(providers["providers"][4]["totalBytes"], 4196);
}

#[test]
fn cli_failures_are_reduced_to_their_error_line() {
    assert_eq!(
        cli::failure_text("WARNING: proceeding\nError: failed to delete session\n", ""),
        "failed to delete session"
    );
    assert_eq!(cli::failure_text("", "\nsomething odd\n"), "something odd");
    assert_eq!(cli::failure_text("", ""), "exited with an error");
    assert!(
        cli::failure_text("Error: key sk-abcdefghijklmnopqrstuvwxyz", "").contains("[REDACTED]")
    );
    let long = format!("Error: {}", "x".repeat(1000));
    assert_eq!(cli::failure_text(&long, "").chars().count(), 301);
    let clis = NativeClis::default();
    let spec = clis.grok_delete(Path::new("/g"), "id1");
    assert_eq!(spec.args, ["--no-auto-update", "sessions", "delete", "id1"]);
    assert_eq!(spec.env["GROK_HOME"], "/g");
    let spec = clis.codex_delete(Path::new("/c"), "id2");
    assert_eq!(spec.args, ["delete", "--force", "id2"]);
    assert_eq!(spec.env["CODEX_HOME"], "/c");
    assert_eq!(
        clis.devin_delete(Path::new("/d"), "id3").args,
        ["rm", "--force", "id3"]
    );
}
