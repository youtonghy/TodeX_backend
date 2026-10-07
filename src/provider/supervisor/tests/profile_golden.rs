//! Golden snapshots of per-provider behaviour. They freeze what clients and
//! native CLIs observe (the `/v2/providers` payload, permission mapping, MCP
//! injection and the supervisor's provider-specific branches) so moving those
//! decisions into `ProviderProfile` cannot change them unnoticed.
//!
//! See [`crate::provider::golden_support`] for regenerating the files.

use super::*;
use crate::provider::golden_support::assert_golden;

const MISSING_BIN: &str = "/nonexistent/todex-golden";
const ACP_PROFILE: &str = "golden";

/// Every CLI points at a missing `MISSING_BIN/<name>`, or at `executable`.
fn golden_config(root: &Path, executable: Option<&str>) -> Config {
    let bin = |name: &str| {
        executable
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{MISSING_BIN}/{name}"))
    };
    Config {
        host: "127.0.0.1".to_owned(),
        port: 0,
        pairing_encryption: PairingEncryption::None,
        data_dir: root.join("data"),
        workspace_roots: vec![root.join("workspaces")],
        history_retention_days: None,
        agent: AgentConfig {
            default_agent: "codex".to_owned(),
            codex_bin: bin("codex"),
            claude_bin: bin("claude"),
            pi_bin: bin("pi"),
            grok_bin: bin("grok"),
            grok_auth_method: None,
            grok_env_allowlist: Vec::new(),
            devin_bin: bin("devin"),
            devin_auth_method: None,
            devin_api_key_env: None,
            devin_env_allowlist: Vec::new(),
            opencode_bin: bin("opencode"),
            opencode_env_allowlist: Vec::new(),
            acp_profiles: BTreeMap::from([(
                ACP_PROFILE.to_owned(),
                AcpProfileConfig {
                    command: bin("acp-agent"),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    auth_method: None,
                    api_key_env: None,
                },
            )]),
            ssh_bin: "ssh".to_owned(),
            provider_idle_timeout_minutes: 0,
        },
        security: SecurityConfig {
            enable_auth: true,
            enable_tls: false,
        },
    }
}

/// A supervisor whose CLIs are all missing (unless `executable` is given),
/// so the snapshot does not depend on what the host has installed. Returns
/// `(root, workspace, supervisor)`.
async fn golden_supervisor(
    label: &str,
    executable: Option<&str>,
) -> (PathBuf, PathBuf, ConversationSupervisor) {
    let root = temp_dir(label);
    let workspace = root.join("workspaces/project");
    fs::create_dir_all(&workspace).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let mut config = golden_config(&root, executable);
    config.workspace_roots = vec![fs::canonicalize(root.join("workspaces")).unwrap()];
    let store = ConversationStore::new(config.data_dir.clone())
        .await
        .unwrap();
    let trust = trust_store(&config, "owner-a", Some(&workspace)).await;
    let supervisor = ConversationSupervisor::new(
        Arc::new(config),
        store,
        ConversationEventHub::default(),
        trust,
    );
    (root, workspace, supervisor)
}

fn error_text(error: AppError) -> Value {
    json!({ "error": error.to_string() })
}

#[tokio::test]
async fn providers_snapshot_matches_golden() {
    let (root, _workspace, supervisor) = golden_supervisor("todex-golden-providers", None).await;
    let providers = supervisor.providers_snapshot().await.unwrap();
    // `/v2/providers` lists every kind once, in `ProviderKind` order.
    let ids: Vec<_> = providers
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["id"].as_str().unwrap().to_owned())
        .collect();
    let expected: Vec<_> = ProviderKind::ALL
        .iter()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    assert_eq!(ids, expected);
    assert_golden("providers", &providers);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn permission_mapping_matches_golden() {
    let modes = [
        None,
        Some("ask"),
        Some("auto"),
        Some("full-access"),
        Some("bogus"),
    ];
    let work_modes = [None, Some("plan")];
    // (profile, sandbox, approval) legacy combinations.
    let legacy = [
        (None, None, None),
        (Some("read-only"), None, None),
        (None, Some("workspace-write"), Some("never")),
        (None, Some("danger-full-access"), Some("on-request")),
    ];
    let mut snapshot = serde_json::Map::new();
    for provider in ProviderKind::ALL {
        let mut cases = serde_json::Map::new();
        for mode in modes {
            for work in work_modes {
                for (profile, sandbox, approval) in legacy {
                    let key = format!(
                        "mode={} work={} profile={} sandbox={} approval={}",
                        mode.unwrap_or("-"),
                        work.unwrap_or("-"),
                        profile.unwrap_or("-"),
                        sandbox.unwrap_or("-"),
                        approval.unwrap_or("-"),
                    );
                    let resolved = crate::provider::types::resolve_execution_config(
                        provider, mode, work, profile, sandbox, approval,
                    )
                    .map(|config| serde_json::to_value(config).unwrap())
                    .unwrap_or_else(error_text);
                    cases.insert(key, resolved);
                }
            }
        }
        snapshot.insert(provider.as_str().to_owned(), Value::Object(cases));
    }
    assert_golden("permission_mapping", &Value::Object(snapshot));
}

/// How each provider receives TodeX's agent MCP servers; `None` when it
/// cannot load servers TodeX supplies.
pub(super) const MCP_INJECTION: [(ProviderKind, Option<&str>); 7] = [
    (ProviderKind::Acp, Some("acp-servers")),
    (ProviderKind::Codex, Some("codex-config")),
    (ProviderKind::Pi, None),
    (ProviderKind::ClaudeCode, Some("claude-args")),
    (ProviderKind::GrokBuild, Some("acp-servers")),
    (ProviderKind::Devin, Some("acp-servers")),
    (ProviderKind::Opencode, Some("acp-servers")),
];

#[cfg(unix)]
#[tokio::test]
async fn agent_mcp_injection_matrix_is_frozen() {
    let (root, _workspace, supervisor) = golden_supervisor("todex-golden-mcp", None).await;
    let ssh = crate::ssh::tests::fixture("Host web\n").await;
    let agent_mcp = crate::agent_mcp::tests::registry(ssh.service.clone(), &root).await;
    ssh.service.set_agent_access("web", true).await.unwrap();
    let supervisor = supervisor.with_agent_mcp(agent_mcp);
    for (provider, format) in MCP_INJECTION {
        let launch = supervisor.agent_mcp_for(provider, "conv_golden").await;
        assert_eq!(launch.is_some(), format.is_some(), "{provider:?}");
    }
    let _ = fs::remove_dir_all(root);
}

/// The supervisor's provider-specific decisions, one row per provider.
#[tokio::test]
async fn supervisor_provider_branches_are_frozen() {
    // Conversations can only be created for an available CLI.
    let executable = std::env::current_exe().unwrap().display().to_string();
    let (root, workspace, supervisor) =
        golden_supervisor("todex-golden-branches", Some(&executable)).await;
    fs::write(workspace.join("notes.txt"), "hello").unwrap();
    let skills = vec![("review".to_owned(), "Review carefully.".to_owned())];
    let agent = &supervisor.config.agent;
    let mut rows = serde_json::Map::new();
    for provider in ProviderKind::ALL {
        let profile = (provider == ProviderKind::Acp).then(|| ACP_PROFILE.to_owned());
        let manifest = supervisor
            .create_owned(
                "owner-a",
                provider,
                workspace.clone(),
                None,
                profile.clone(),
            )
            .await
            .unwrap();
        let (file_text, file_content) = prepare_prompt_content(
            provider,
            &workspace,
            vec![PromptContentRef::File {
                path: PathBuf::from("notes.txt"),
                name: None,
            }],
        )
        .await
        .unwrap();
        let profiles = vec![ACP_PROFILE.to_owned()];
        let profile_result = |requested: Option<&str>| {
            normalize_profile(provider, requested.map(str::to_owned), &profiles)
                .map(|profile| json!(profile))
                .unwrap_or_else(error_text)
        };
        let row = json!({
            "recoveryIsSettled": supervisor.recovery_is_settled(&manifest).await,
            "promptWithSkills": provider_prompt_text(provider, "do it", &skills),
            "attachedFileText": file_text,
            "attachedFileItems": file_content.len(),
            "typedImages": ensure_image_provider(provider).map(|()| json!(true)).unwrap_or_else(error_text),
            "profileExplicit": profile_result(Some(ACP_PROFILE)),
            "profileOmitted": profile_result(None),
            "runsAcpProfileCli": turn_runs_cli(
                agent,
                provider,
                Some(ACP_PROFILE),
                ProviderKind::Codex,
                &executable,
            ),
            "runsOwnCli": turn_runs_cli(agent, provider, None, provider, "unused"),
            "runsOtherCli": turn_runs_cli(
                agent,
                provider,
                None,
                if provider == ProviderKind::Codex { ProviderKind::Pi } else { ProviderKind::Codex },
                "/nonexistent/other-cli",
            ),
        });
        rows.insert(provider.as_str().to_owned(), row);
    }
    assert_golden("supervisor_branches", &Value::Object(rows));
    let _ = fs::remove_dir_all(root);
}
