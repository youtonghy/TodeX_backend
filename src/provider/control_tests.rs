//! Public supervisor contracts against a deterministic native RPC process.
#![cfg(unix)]

use super::{types::ProviderControl, ConversationPrompt, ConversationSupervisor};
use crate::{
    config::{AgentConfig, Config, PairingEncryption, SecurityConfig},
    conversation::{
        ConversationEvent, ConversationEventHub, ConversationManifest, ConversationStore,
        ProviderKind,
    },
    error::AppError,
    workspace_trust::WorkspaceTrustStore,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

struct Harness {
    root: PathBuf,
    supervisor: ConversationSupervisor,
    store: ConversationStore,
    trust: WorkspaceTrustStore,
    manifest: ConversationManifest,
}

impl Harness {
    async fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "todex-control-contract-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(root.join("workspace"))
            .await
            .unwrap();
        let root = tokio::fs::canonicalize(root).await.unwrap();
        let script = root.join("pi.py");
        tokio::fs::write(
            &script,
            include_str!("../../tests/fixtures/pi_rpc_audit.py"),
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let executable = script.display().to_string();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root: root.clone(),
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "pi".to_owned(),
                pi_bin: executable.clone(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: vec![],
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("fixture".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = WorkspaceTrustStore::new(root.join("trust"), root.clone())
            .await
            .unwrap();
        trust
            .set_owned("owner-a", &root.join("workspace"), true)
            .await
            .unwrap();
        let supervisor = ConversationSupervisor::new(
            config,
            store.clone(),
            ConversationEventHub::default(),
            trust.clone(),
        );
        let manifest = supervisor
            .create_owned(
                "owner-a",
                ProviderKind::Pi,
                root.join("workspace"),
                None,
                None,
            )
            .await
            .unwrap();
        Self {
            root,
            supervisor,
            store,
            trust,
            manifest,
        }
    }

    fn prompt(id: &str, text: &str) -> ConversationPrompt {
        ConversationPrompt {
            client_request_id: Some(id.to_owned()),
            text: text.to_owned(),
            model: None,
            reasoning_effort: None,
            skills: vec![],
            content: vec![],
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        }
    }

    async fn start(&self) -> String {
        let turn = self
            .supervisor
            .prompt_owned(
                "owner-a",
                &self.manifest.id,
                Self::prompt("submission", "hold"),
            )
            .await
            .unwrap();
        // Starting the fixture includes a cold Python launch and durable session setup.
        // Give CI runners room to initialize without relaxing live control deadlines.
        self.wait_event_with_timeout("tool.started", None, Duration::from_secs(30))
            .await;
        turn
    }

    async fn events(&self) -> Vec<ConversationEvent> {
        self.store
            .complete_history(&self.manifest.id)
            .await
            .unwrap()
    }

    async fn wait_event(&self, kind: &str, request: Option<&str>) {
        self.wait_event_with_timeout(kind, request, Duration::from_secs(5))
            .await;
    }

    async fn wait_event_with_timeout(&self, kind: &str, request: Option<&str>, timeout: Duration) {
        let result = tokio::time::timeout(timeout, async {
            loop {
                if self.events().await.iter().any(|event| {
                    event.event_type == kind
                        && request.is_none_or(|id| {
                            event.payload.get("requestId").and_then(Value::as_str) == Some(id)
                        })
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if result.is_err() {
            panic!(
                "missing {kind} {request:?} within {timeout:?}; events: {:#?}; native commands: {:#?}",
                self.events().await,
                self.native_commands().await,
            );
        }
    }

    async fn native_commands(&self) -> Vec<Value> {
        tokio::fs::read_to_string(self.root.join("workspace/pi-commands"))
            .await
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn finish(self) {
        self.supervisor.shutdown_all().await;
        tokio::fs::remove_dir_all(self.root).await.unwrap();
    }
}

#[tokio::test]
async fn controls_check_owner_trust_and_stale_turn_before_native_mutation() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    let command = ProviderControl::Steer {
        text: "must not send".to_owned(),
    };
    assert!(harness
        .supervisor
        .control_owned(
            "owner-b",
            &harness.manifest.id,
            &turn,
            "wrong-owner",
            command.clone()
        )
        .await
        .is_err());
    harness
        .trust
        .set_owned("owner-a", &harness.manifest.workspace, false)
        .await
        .unwrap();
    assert!(matches!(
        harness
            .supervisor
            .control_owned(
                "owner-a",
                &harness.manifest.id,
                &turn,
                "untrusted",
                command.clone()
            )
            .await,
        Err(AppError::WorkspaceTrustRequired(_))
    ));
    harness
        .trust
        .set_owned("owner-a", &harness.manifest.workspace, true)
        .await
        .unwrap();
    assert!(matches!(
        harness
            .supervisor
            .control_owned(
                "owner-a",
                &harness.manifest.id,
                "stale-turn",
                "stale",
                command
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(!harness
        .events()
        .await
        .iter()
        .any(|event| event.event_type == "control.requested"));
    assert!(!harness
        .native_commands()
        .await
        .iter()
        .any(|command| command.get("type") == Some(&json!("steer"))));
    harness.finish().await;
}

#[tokio::test]
async fn duplicate_controls_are_once_only_and_rejections_are_durable() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    let command = ProviderControl::QueueAdd {
        item_id: "queue-item".to_owned(),
        text: "queued message".to_owned(),
    };
    let (first, repeated) = tokio::join!(
        harness.supervisor.control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "once",
            command.clone()
        ),
        harness.supervisor.control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "once",
            command.clone()
        ),
    );
    assert_eq!(first.unwrap(), repeated.unwrap());
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("type") == Some(&json!("follow_up")))
            .count(),
        1
    );
    assert!(matches!(
        harness
            .supervisor
            .control_owned(
                "owner-a",
                &harness.manifest.id,
                &turn,
                "once",
                ProviderControl::Steer {
                    text: "different".to_owned()
                }
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let rejected = ProviderControl::Configure {
        model: Some("fixture/reject".to_owned()),
        reasoning_effort: None,
    };
    assert!(harness
        .supervisor
        .control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "rejected",
            rejected.clone()
        )
        .await
        .is_err());
    assert!(harness
        .supervisor
        .control_owned("owner-a", &harness.manifest.id, &turn, "rejected", rejected)
        .await
        .is_err());
    let events = harness.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "control.rejected"
                && event.payload.get("requestId") == Some(&json!("rejected")))
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|event| event.event_type == "turn.configuration"
            && event.payload.pointer("/effective/model") == Some(&json!("fixture/text"))));
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("type") == Some(&json!("set_model")))
            .count(),
        1
    );
    harness.finish().await;
}

#[tokio::test]
async fn lost_control_waiter_does_not_resend_and_outcome_is_still_recorded() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    let supervisor = harness.supervisor.clone();
    let conversation = harness.manifest.id.clone();
    let target = turn.clone();
    let waiting = tokio::spawn(async move {
        supervisor
            .control_owned(
                "owner-a",
                &conversation,
                &target,
                "lost-ack",
                ProviderControl::Steer {
                    text: "delayed".to_owned(),
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !harness
            .native_commands()
            .await
            .iter()
            .any(|command| command.get("id") == Some(&json!("lost-ack")))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    waiting.abort();
    harness
        .wait_event("control.completed", Some("lost-ack"))
        .await;
    harness
        .supervisor
        .control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "lost-ack",
            ProviderControl::Steer {
                text: "delayed".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("id") == Some(&json!("lost-ack")))
            .count(),
        1
    );
    harness.finish().await;
}

#[tokio::test]
async fn prompt_request_id_replay_preserves_identity_and_rejects_changed_input() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    assert_eq!(
        harness
            .supervisor
            .prompt_owned(
                "owner-a",
                &harness.manifest.id,
                Harness::prompt("submission", "hold")
            )
            .await
            .unwrap(),
        turn
    );
    assert!(matches!(
        harness
            .supervisor
            .prompt_owned(
                "owner-a",
                &harness.manifest.id,
                Harness::prompt("submission", "different")
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("type") == Some(&json!("prompt")))
            .count(),
        1
    );
    assert!(harness
        .supervisor
        .prompt_owned(
            "owner-b",
            &harness.manifest.id,
            Harness::prompt("submission", "hold")
        )
        .await
        .is_err());
    harness.finish().await;
}

#[tokio::test]
async fn disconnected_native_transport_records_unknown_without_reexecution() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    let command = ProviderControl::Steer {
        text: "disconnect".to_owned(),
    };
    assert!(harness
        .supervisor
        .control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "unknown",
            command.clone()
        )
        .await
        .is_err());
    harness.wait_event("control.unknown", Some("unknown")).await;
    assert!(matches!(
        harness
            .supervisor
            .control_owned("owner-a", &harness.manifest.id, &turn, "unknown", command)
            .await,
        Err(AppError::ProviderUnavailable(_))
    ));
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("id") == Some(&json!("unknown")))
            .count(),
        1
    );
    assert!(!harness
        .events()
        .await
        .iter()
        .any(|event| event.event_type == "control.rejected"
            && event.payload.get("requestId") == Some(&json!("unknown"))));
    harness.finish().await;
}

#[tokio::test]
async fn consumed_native_queue_id_cannot_be_replayed_as_a_new_prompt() {
    let harness = Harness::new().await;
    let turn = harness.start().await;
    harness
        .supervisor
        .control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "add",
            ProviderControl::QueueAdd {
                item_id: "delivered-id".to_owned(),
                text: "follow up".to_owned(),
            },
        )
        .await
        .unwrap();
    harness
        .supervisor
        .control_owned(
            "owner-a",
            &harness.manifest.id,
            &turn,
            "finish",
            ProviderControl::Steer {
                text: "finish".to_owned(),
            },
        )
        .await
        .unwrap();
    harness.wait_event("turn.completed", None).await;
    assert_eq!(
        harness
            .supervisor
            .prompt_owned(
                "owner-a",
                &harness.manifest.id,
                Harness::prompt("delivered-id", "follow up")
            )
            .await
            .unwrap(),
        turn
    );
    assert_eq!(
        harness
            .native_commands()
            .await
            .iter()
            .filter(|command| command.get("type") == Some(&json!("prompt")))
            .count(),
        1
    );
    assert!(matches!(
        harness
            .supervisor
            .prompt_owned(
                "owner-a",
                &harness.manifest.id,
                Harness::prompt("delivered-id", "changed")
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    harness.finish().await;
}
