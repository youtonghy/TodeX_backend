//! Claude provider profiles own only the identity keys of `settings.json`:
//! keys added outside the provider form (hooks, status line, permissions,
//! other `env` variables) stay in the live file through every activation,
//! save and deletion, and are never copied into a profile.

use super::*;
use serde_json::json;
use std::path::{Path, PathBuf};

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-merge-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dirs_in(root: &Path) -> AgentDirs {
    AgentDirs {
        codex_home: root.join("codex"),
        claude_dir: root.join("claude"),
        grok_home: root.join("grok"),
        grok_auth_path: root.join("grok").join("auth.json"),
        pi_dir: root.join("pi"),
        opencode_dir: root.join("opencode"),
        dsh_home: root.join("dsh"),
    }
}

async fn service_in(root: &Path) -> AgentProviderService {
    AgentProviderService::with_dirs(root.join("data"), Some(dirs_in(root)))
        .await
        .unwrap()
}

fn input(name: &str, settings_config: Value) -> AgentProviderInput {
    AgentProviderInput {
        name: name.to_owned(),
        settings_config,
        website_url: None,
        category: None,
        notes: None,
        icon: None,
        icon_color: None,
        sort_index: None,
    }
}

fn profile(base: &str, token: &str) -> Value {
    json!({
        "env": {
            "ANTHROPIC_BASE_URL": base,
            "ANTHROPIC_AUTH_TOKEN": token,
            "ANTHROPIC_MODEL": "claude-opus-4-1"
        },
        "permissions": {"allow": ["Bash"]}
    })
}

/// Keys the provider form does not know about, added by hand / CLI settings.
fn add_foreign(live: &mut Value) {
    live["statusLine"] = json!({"type": "command", "command": "echo hi"});
    live["hooks"] = json!({"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo pre"}]}]});
    live["permissions"]["deny"] = json!(["Bash(rm:*)"]);
    live["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = json!("64000");
}

/// Which of the foreign keys a settings blob carries.
#[derive(Debug, PartialEq)]
struct Probe {
    status_line: bool,
    hooks: bool,
    deny: bool,
    env_custom: bool,
}

impl Probe {
    fn of(value: Option<&Value>) -> Self {
        let value = value.cloned().unwrap_or(Value::Null);
        Self {
            status_line: value.get("statusLine").is_some(),
            hooks: value.get("hooks").is_some(),
            deny: value.pointer("/permissions/deny").is_some(),
            env_custom: value
                .pointer("/env/CLAUDE_CODE_MAX_OUTPUT_TOKENS")
                .is_some(),
        }
    }
    fn all() -> Self {
        Self {
            status_line: true,
            hooks: true,
            deny: true,
            env_custom: true,
        }
    }
    fn none() -> Self {
        Self {
            status_line: false,
            hooks: false,
            deny: false,
            env_custom: false,
        }
    }
}

fn live(dirs: &AgentDirs) -> Option<Value> {
    claude::read_live(dirs).unwrap()
}

fn live_probe(dirs: &AgentDirs) -> Probe {
    Probe::of(live(dirs).as_ref())
}

fn claude_dir_entries(dirs: &AgentDirs) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&dirs.claude_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn backups(dirs: &AgentDirs) -> Vec<String> {
    claude_dir_entries(dirs)
        .into_iter()
        .filter(|name| name.starts_with("settings.json.todex-backup-"))
        .collect()
}

async fn stored(service: &AgentProviderService, id: &str) -> Option<Value> {
    service
        .store
        .profile(ProviderKind::ClaudeCode, id)
        .await
        .map(|profile| profile.settings_config)
}

async fn stored_probe(service: &AgentProviderService, id: &str) -> Probe {
    Probe::of(stored(service, id).await.as_ref())
}

/// The whole provider store, serialized, to check no copy of a value hides
/// in any profile.
async fn store_text(service: &AgentProviderService) -> String {
    serde_json::to_string(&service.store.snapshot().await).unwrap()
}

async fn activate(service: &AgentProviderService, id: &str) {
    service
        .activate(ProviderKind::ClaudeCode, id, None)
        .await
        .unwrap();
}

async fn seed_two_profiles(service: &AgentProviderService) {
    service
        .upsert(
            ProviderKind::ClaudeCode,
            "a",
            input("A", profile("https://a.example", "key-a")),
        )
        .await
        .unwrap();
    service
        .upsert(
            ProviderKind::ClaudeCode,
            "b",
            input("B", profile("https://b.example", "key-b")),
        )
        .await
        .unwrap();
}

/// Writes foreign keys into the live file on top of whatever is there, the
/// way the CLI settings page (or a hand edit) would.
fn edit_live_by_hand(dirs: &AgentDirs) {
    let mut value = live(dirs).unwrap_or_else(|| json!({"env": {}, "permissions": {}}));
    if value.get("permissions").is_none() {
        value["permissions"] = json!({});
    }
    if value.get("env").is_none() {
        value["env"] = json!({});
    }
    add_foreign(&mut value);
    claude::write_live(dirs, &value).unwrap();
}

#[tokio::test]
async fn first_activation_keeps_live_keys_and_imports_unknown_credentials() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    // A pre-existing hand-written settings.json, before TodeX manages Claude.
    let mut original = json!({
        "env": {"ANTHROPIC_BASE_URL": "https://orig.example", "ANTHROPIC_AUTH_TOKEN": "orig-key"},
        "permissions": {}
    });
    add_foreign(&mut original);
    claude::write_live(&dirs, &original).unwrap();
    let original_text = std::fs::read_to_string(claude::settings_path(&dirs)).unwrap();
    seed_two_profiles(&service).await;

    activate(&service, "a").await;
    let now = live(&dirs).unwrap();
    assert_eq!(live_probe(&dirs), Probe::all(), "foreign keys stay live");
    assert_eq!(now["env"]["ANTHROPIC_AUTH_TOKEN"], "key-a");
    assert_eq!(now["env"]["ANTHROPIC_BASE_URL"], "https://a.example");
    assert!(
        now.pointer("/permissions/allow").is_none(),
        "unmanaged profile keys are not projected"
    );
    assert_eq!(stored_probe(&service, "a").await, Probe::none());
    assert_eq!(stored_probe(&service, "b").await, Probe::none());

    // The original credentials became an "Imported" profile, managed keys only.
    let imported = service
        .store
        .profile(ProviderKind::ClaudeCode, "imported")
        .await
        .expect("live credentials imported");
    assert_eq!(imported.name, "Imported");
    assert_eq!(
        imported.settings_config,
        json!({"env": {"ANTHROPIC_BASE_URL": "https://orig.example", "ANTHROPIC_AUTH_TOKEN": "orig-key"}})
    );
    assert_eq!(
        service
            .bucket(ProviderKind::ClaudeCode)
            .await
            .current_provider_id
            .as_deref(),
        Some("a")
    );
    // And the original file was backed up byte for byte, once.
    let backups = backups(&dirs);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        std::fs::read_to_string(dirs.claude_dir.join(&backups[0])).unwrap(),
        original_text
    );
}

#[tokio::test]
async fn first_activation_over_the_profile_limit_only_backs_up() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    for index in 0..MAX_PROVIDERS_PER_AGENT {
        service
            .upsert(
                ProviderKind::ClaudeCode,
                &format!("p{index}"),
                input("P", profile("https://p.example", "key-p")),
            )
            .await
            .unwrap();
    }
    claude::write_live(
        &dirs,
        &json!({"env": {"ANTHROPIC_AUTH_TOKEN": "orig-key"}, "hooks": {}}),
    )
    .unwrap();

    activate(&service, "p0").await;
    let bucket = service.bucket(ProviderKind::ClaudeCode).await;
    assert_eq!(bucket.providers.len(), MAX_PROVIDERS_PER_AGENT);
    assert!(!store_text(&service).await.contains("orig-key"));
    let backups = backups(&dirs);
    assert_eq!(backups.len(), 1, "backed up once: {backups:?}");
    assert!(std::fs::read_to_string(dirs.claude_dir.join(&backups[0]))
        .unwrap()
        .contains("orig-key"));
    let now = live(&dirs).unwrap();
    assert_eq!(now["env"]["ANTHROPIC_AUTH_TOKEN"], "key-p");
    assert!(now.get("hooks").is_some());
}

#[tokio::test]
async fn switching_a_b_a_keeps_live_keys_out_of_profiles() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs);

    activate(&service, "b").await;
    assert_eq!(live_probe(&dirs), Probe::all(), "still live under B");
    assert_eq!(live(&dirs).unwrap()["env"]["ANTHROPIC_AUTH_TOKEN"], "key-b");
    assert_eq!(
        stored_probe(&service, "a").await,
        Probe::none(),
        "A is not backfilled with them"
    );
    assert_eq!(
        stored(&service, "a").await.unwrap()["permissions"],
        json!({"allow": ["Bash"]}),
        "A's own unmanaged keys are kept, not migrated"
    );

    activate(&service, "a").await;
    assert_eq!(live_probe(&dirs), Probe::all());
    assert_eq!(live(&dirs).unwrap()["env"]["ANTHROPIC_AUTH_TOKEN"], "key-a");
    assert_eq!(claude_dir_entries(&dirs), vec!["settings.json".to_owned()]);
}

#[tokio::test]
async fn custom_env_key_is_one_live_copy_across_profiles() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs); // CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000 while A
    activate(&service, "b").await;
    assert_eq!(
        live(&dirs).unwrap()["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"],
        "64000",
        "visible under B"
    );
    // The user changes it while B is current.
    let mut value = live(&dirs).unwrap();
    value["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = json!("8000");
    claude::write_live(&dirs, &value).unwrap();

    activate(&service, "a").await;
    assert_eq!(
        live(&dirs).unwrap()["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"],
        "8000",
        "the latest edit wins under A"
    );
    activate(&service, "b").await;
    assert_eq!(
        live(&dirs).unwrap()["env"]["CLAUDE_CODE_MAX_OUTPUT_TOKENS"],
        "8000"
    );
    assert!(
        !store_text(&service)
            .await
            .contains("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        "no per-profile copy"
    );
}

#[tokio::test]
async fn reactivating_current_keeps_live_keys() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs);
    activate(&service, "a").await;
    assert_eq!(live_probe(&dirs), Probe::all());
    assert_eq!(stored_probe(&service, "a").await, Probe::none());
    assert_eq!(claude_dir_entries(&dirs), vec!["settings.json".to_owned()]);
}

#[tokio::test]
async fn reactivating_current_backs_up_a_hand_edited_credential() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    let mut value = live(&dirs).unwrap();
    value["env"]["ANTHROPIC_AUTH_TOKEN"] = json!("hand-key");
    claude::write_live(&dirs, &value).unwrap();

    activate(&service, "a").await;
    assert_eq!(live(&dirs).unwrap()["env"]["ANTHROPIC_AUTH_TOKEN"], "key-a");
    let backups = backups(&dirs);
    assert_eq!(backups.len(), 1);
    assert!(std::fs::read_to_string(dirs.claude_dir.join(&backups[0]))
        .unwrap()
        .contains("hand-key"));
}

#[tokio::test]
async fn saving_current_profile_keeps_live_keys() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs);
    // The desktop form: `{...previous, env: {...previous.env, ANTHROPIC_MODEL}}`
    // with `previous` = masked stored profile.
    let block = service
        .snapshot(Some(ProviderKind::ClaudeCode))
        .await
        .unwrap();
    let masked_a = block["agents"]["claude-code"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["id"] == "a")
        .unwrap()["settingsConfig"]
        .clone();
    assert_eq!(masked_a["env"]["ANTHROPIC_AUTH_TOKEN"], MASKED_SECRET);
    let mut edited = masked_a;
    edited["env"]["ANTHROPIC_MODEL"] = json!("claude-sonnet-4-5");
    service
        .upsert(ProviderKind::ClaudeCode, "a", input("A", edited))
        .await
        .unwrap();
    assert_eq!(live_probe(&dirs), Probe::all());
    let now = live(&dirs).unwrap();
    assert_eq!(now["env"]["ANTHROPIC_MODEL"], "claude-sonnet-4-5");
    assert_eq!(now["env"]["ANTHROPIC_AUTH_TOKEN"], "key-a");
    assert_eq!(stored_probe(&service, "a").await, Probe::none());
    assert_eq!(claude_dir_entries(&dirs), vec!["settings.json".to_owned()]);
    let block = service.live(ProviderKind::ClaudeCode).await.unwrap();
    assert_eq!(
        block["matchesCurrent"], true,
        "foreign keys do not count as drift"
    );
}

#[tokio::test]
async fn deleting_current_strips_its_credentials_and_keeps_live_keys() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs);
    service.delete(ProviderKind::ClaudeCode, "a").await.unwrap();
    assert_eq!(live_probe(&dirs), Probe::all());
    let left = live(&dirs).unwrap();
    assert!(
        !left.to_string().contains("ANTHROPIC_"),
        "deleted profile's keys are gone: {left}"
    );
    assert!(service
        .bucket(ProviderKind::ClaudeCode)
        .await
        .current_provider_id
        .is_none());
    assert_eq!(
        service.live(ProviderKind::ClaudeCode).await.unwrap()["matchesCurrent"],
        false
    );
    assert!(backups(&dirs).is_empty(), "the deleted values were in A");

    activate(&service, "b").await;
    assert_eq!(live_probe(&dirs), Probe::all());
    assert_eq!(live(&dirs).unwrap()["env"]["ANTHROPIC_AUTH_TOKEN"], "key-b");
    assert_eq!(stored_probe(&service, "b").await, Probe::none());
    assert!(
        stored(&service, "imported").await.is_none(),
        "nothing to import"
    );
    assert_eq!(claude_dir_entries(&dirs), vec!["settings.json".to_owned()]);
}

#[tokio::test]
async fn deleting_a_non_current_profile_leaves_live_alone() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    edit_live_by_hand(&dirs);
    activate(&service, "b").await;
    let before = std::fs::read(claude::settings_path(&dirs)).unwrap();
    service.delete(ProviderKind::ClaudeCode, "a").await.unwrap();
    assert_eq!(std::fs::read(claude::settings_path(&dirs)).unwrap(), before);
    assert_eq!(live_probe(&dirs), Probe::all());
    assert!(!store_text(&service).await.contains("echo hi"));
}

#[tokio::test]
async fn import_live_captures_only_the_managed_keys() {
    let root = Root::new();
    let service = service_in(&root.0).await;
    let dirs = dirs_in(&root.0);
    edit_live_by_hand(&dirs);
    let mut value = live(&dirs).unwrap();
    value["env"]["ANTHROPIC_AUTH_TOKEN"] = json!("live-key");
    value["model"] = json!("opus");
    claude::write_live(&dirs, &value).unwrap();
    service
        .import_live(
            ProviderKind::ClaudeCode,
            ImportLiveInput {
                id: "imported".into(),
                name: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stored(&service, "imported").await.unwrap(),
        json!({"model": "opus", "env": {"ANTHROPIC_AUTH_TOKEN": "live-key"}})
    );
    assert_eq!(
        service.live(ProviderKind::ClaudeCode).await.unwrap()["matchesCurrent"],
        true
    );
    seed_two_profiles(&service).await;
    activate(&service, "a").await;
    assert_eq!(live_probe(&dirs), Probe::all());
    assert!(
        live(&dirs).unwrap().get("model").is_none(),
        "the outgoing profile's model is not left behind"
    );
    assert_eq!(stored_probe(&service, "imported").await, Probe::none());
}
