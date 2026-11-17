//! Real Codex app-server runs through TodeX's Codex driver (the path the
//! `/v2` conversation API drives): a config warning, a command approval with
//! Codex's `availableDecisions`, auto-review in `auto` mode, a file change,
//! steering and interrupting a running turn, compaction and fork. Billable;
//! run with `TODEX_REAL_E2E=1 TODEX_REAL_ALLOW_BILLABLE=1` and, optionally,
//! `TODEX_REAL_CODEX_BIN`:
//! `cargo test --bin todex-agentd real_codex_driver_common_interfaces -- --ignored --nocapture`.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use super::*;
use crate::conversation::{
    ConversationEvent, ConversationEventHub, ConversationManifest, ConversationStore,
};
use crate::provider::types::{
    DriverContext, DriverEventSink, DriverPrompt, PermissionBroker, PermissionDecision,
    PermissionPolicy, ProviderControl,
};

/// Answers every request with its `allow_once` option, as a user would.
struct AllowOnce;

#[async_trait]
impl PermissionPolicy for AllowOnce {
    async fn decide(
        &self,
        _conversation_id: &str,
        options: &Value,
        _device_bound: bool,
    ) -> Option<(PermissionDecision, String)> {
        let option = options
            .as_array()?
            .iter()
            .find(|option| option["kind"] == "allow_once")?;
        Some((
            PermissionDecision {
                outcome: PermissionOutcome::AllowOnce,
                option_id: option["optionId"].as_str().map(ToOwned::to_owned),
                data: None,
            },
            "real-e2e".to_owned(),
        ))
    }
}

struct Harness {
    driver: CodexDriver,
    store: ConversationStore,
    hub: ConversationEventHub,
    broker: PermissionBroker,
    workspace: std::path::PathBuf,
}

impl Harness {
    async fn conversation(&self) -> String {
        self.store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                self.workspace.clone(),
                None,
                None,
            ))
            .await
            .unwrap()
            .id
    }

    async fn context(&self, id: &str) -> DriverContext {
        DriverContext {
            manifest: self.store.get(id).await.unwrap(),
            provider_state: self.store.provider_state(id).await.unwrap(),
            agent_mcp: None,
            handles: Default::default(),
        }
    }

    fn sink(&self, id: &str, turn: &str) -> DriverEventSink {
        DriverEventSink::new(
            self.store.clone(),
            self.hub.clone(),
            self.broker.clone(),
            id.to_owned(),
        )
        .with_turn_id(turn)
    }

    /// Runs one turn; `during` sees each live event and may act on the turn
    /// (steer, interrupt). Returns the turn's journalled events.
    async fn turn(
        &self,
        id: &str,
        turn: &str,
        prompt: DriverPrompt,
        mut during: impl FnMut(&ConversationEvent, &watch::Sender<bool>) -> Option<ProviderControl>,
    ) -> (
        Result<super::super::types::DriverTurnResult, AppError>,
        Vec<ConversationEvent>,
    ) {
        let mut live = self.hub.subscribe(id);
        let (cancel_tx, cancel) = watch::channel(false);
        let context = self.context(id).await;
        let run = self
            .driver
            .run_turn(context, prompt, self.sink(id, turn), cancel);
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                event = live.recv() => {
                    let Ok(event) = event else { continue };
                    if let Some(control) = during(&event, &cancel_tx) {
                        let answer = self.driver.control(id, turn, &format!("control-{turn}"), control).await;
                        eprintln!("control answer: {answer:?}");
                    }
                }
            }
        };
        let events = self
            .store
            .complete_history(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| {
                event.payload["turnId"]
                    .as_str()
                    .is_none_or(|owner| owner == turn)
            })
            .collect();
        (result, events)
    }
}

fn prompt(turn: &str, text: &str) -> DriverPrompt {
    DriverPrompt {
        turn_id: turn.into(),
        text: text.into(),
        content: vec![],
        skills: vec![],
        model: None,
        reasoning_effort: None,
        permission_mode: None,
        work_mode: None,
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
        after_plan: None,
        replan: None,
    }
}

fn of<'a>(events: &'a [ConversationEvent], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event.event_type == event_type)
        .map(|event| &event.payload)
        .collect()
}

fn notices<'a>(events: &'a [ConversationEvent], method: &str) -> Vec<&'a Value> {
    of(events, "provider.event")
        .into_iter()
        .filter(|payload| payload["providerMethod"] == method)
        .map(|payload| &payload["metadata"])
        .collect()
}

fn reply(events: &[ConversationEvent]) -> Option<String> {
    of(events, "message.completed").last().map(|payload| {
        payload
            .pointer("/message/text")
            .or_else(|| payload.get("text"))
            .or_else(|| payload.get("content"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| payload.to_string())
    })
}

/// A private `CODEX_HOME` with the user's login and config (without MCP
/// servers), plus an unknown key so Codex 0.169+ sends `configWarning`.
fn codex_home(root: &std::path::Path) -> std::path::PathBuf {
    let source = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".codex")
        });
    let home = root.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    for file in ["auth.json", "installation_id"] {
        if source.join(file).exists() {
            std::fs::copy(source.join(file), home.join(file)).unwrap();
        }
    }
    let mut config = std::fs::read_to_string(source.join("config.toml"))
        .unwrap_or_default()
        .parse::<toml::Table>()
        .unwrap_or_default();
    config.remove("mcp_servers");
    config.remove("marketplaces");
    config.insert(
        "todex_e2e_unknown_setting".into(),
        toml::Value::Boolean(true),
    );
    std::fs::write(home.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    home
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "billable: requires TODEX_REAL_E2E=1, TODEX_REAL_ALLOW_BILLABLE=1, a Codex login and model access"]
async fn real_codex_driver_common_interfaces() {
    assert_eq!(std::env::var("TODEX_REAL_E2E").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("TODEX_REAL_ALLOW_BILLABLE").as_deref(),
        Ok("1")
    );
    let root = std::env::temp_dir().join(format!(
        "todex-real-codex-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = std::fs::canonicalize(workspace).unwrap();
    let home = codex_home(&root);
    // SAFETY: this ignored test runs alone (`--ignored` with a filter); the
    // Codex child processes inherit CODEX_HOME.
    unsafe { std::env::set_var("CODEX_HOME", &home) };
    let binary = std::env::var("TODEX_REAL_CODEX_BIN").unwrap_or_else(|_| "codex".to_owned());
    let broker = PermissionBroker::default();
    broker.set_policy(Arc::new(AllowOnce));
    let h = Harness {
        driver: CodexDriver {
            binary,
            sessions: runtime::Sessions::new(),
            control_probe: ControlProbeCache::default(),
            models: CatalogCache::new(PROFILE.discovery_cache_ttl),
        },
        store: ConversationStore::new(root.join("data")).await.unwrap(),
        hub: ConversationEventHub::default(),
        broker,
        workspace: workspace.clone(),
    };
    let mut report = serde_json::Map::new();

    // 0. Model catalog.
    let models = h.driver.discover_models(&workspace).await.unwrap();
    report.insert(
        "models".into(),
        json!(models.iter().map(|model| json!({
        "id": model.id, "default": model.is_default, "efforts": model.supported_reasoning_efforts,
    })).collect::<Vec<_>>()),
    );

    // 1. Read-only sandbox: writing a file needs approval. The policy picks
    // allow_once; the file then exists.
    let first = h.conversation().await;
    let target = workspace.join("todex_e2e_approval.txt");
    let (result, events) = h
        .turn(
            &first,
            "turn-approval",
            DriverPrompt {
                sandbox_mode: Some("read-only".into()),
                approval_policy: Some("on-request".into()),
                ..prompt("turn-approval", "Run exactly this shell command and nothing else: echo todex > todex_e2e_approval.txt\nIf it needs approval, request it. Then reply with the single word DONE.")
            },
            |_, _| None,
        )
        .await;
    let approvals = of(&events, "permission.requested");
    report.insert("approval".into(), json!({
        "result": format!("{result:?}"),
        "requests": approvals.iter().map(|payload| json!({
            "kind": payload["kind"], "title": payload["title"],
            "options": payload["options"].as_array().map(|options| options.iter().map(|option| &option["kind"]).collect::<Vec<_>>()),
            "availableDecisions": payload["details"]["availableDecisions"],
        })).collect::<Vec<_>>(),
        "fileWritten": target.exists(),
        "reply": reply(&events),
    }));
    report.insert(
        "warnings".into(),
        json!(notices(&events, "provider_warning")),
    );

    // 2. auto: Codex's reviewer judges a command the sandbox blocks.
    let auto = h.conversation().await;
    let (result, events) = h
        .turn(
            &auto,
            "turn-auto",
            DriverPrompt {
                permission_mode: Some("auto".into()),
                ..prompt("turn-auto", "Run this shell command: curl -sS -o /dev/null -w '%{http_code}' https://example.com\nIf the sandbox blocks network access, request escalated permissions to run it. Reply with the HTTP status code only.")
            },
            |_, _| None,
        )
        .await;
    report.insert(
        "autoReview".into(),
        json!({
            "result": format!("{result:?}"),
            "notices": notices(&events, "auto_review"),
            "personAsked": of(&events, "permission.requested").len(),
            "reply": reply(&events),
        }),
    );

    // 3. A file change, steered while its command runs.
    let edits = h.conversation().await;
    let notes = workspace.join("todex_e2e_notes.md");
    let mut steered = false;
    let (result, events) = h
        .turn(
            &edits,
            "turn-edit",
            DriverPrompt {
                permission_mode: Some("full-access".into()),
                ..prompt("turn-edit", "Create the file todex_e2e_notes.md containing one line: hello from todex. Then run the shell command `sleep 8`. Then reply with the word FINISHED.")
            },
            |event, _| {
                (!steered && event.event_type == "tool.started" && event.payload.to_string().contains("sleep")).then(|| {
                    steered = true;
                    ProviderControl::Steer { text: "Also append the word STEERED to the end of your final reply.".into() }
                })
            },
        )
        .await;
    report.insert("editAndSteer".into(), json!({
        "result": format!("{result:?}"),
        "file": std::fs::read_to_string(&notes).ok(),
        "tools": of(&events, "tool.completed").iter().map(|payload| payload["item"]["type"].clone()).collect::<Vec<_>>(),
        "steered": steered,
        "reply": reply(&events),
    }));

    // 4. Interrupt a running command.
    let mut interrupted = false;
    let (result, _) = h
        .turn(
            &edits,
            "turn-interrupt",
            DriverPrompt {
                permission_mode: Some("full-access".into()),
                ..prompt(
                    "turn-interrupt",
                    "Run the shell command `sleep 60`, then reply DONE.",
                )
            },
            |event, cancel| {
                if !interrupted && event.event_type == "tool.started" {
                    interrupted = true;
                    let _ = cancel.send(true);
                }
                None
            },
        )
        .await;
    report.insert(
        "interrupt".into(),
        json!({"interrupted": interrupted, "result": format!("{result:?}")}),
    );

    // 5. Compaction, then fork.
    let (_cancel_compaction, cancel) = watch::channel(false);
    let compact = tokio::time::timeout(
        Duration::from_secs(300),
        h.driver
            .compact_session(h.context(&first).await, None, cancel),
    )
    .await;
    report.insert("compact".into(), json!(format!("{compact:?}")));
    let fork = h.driver.fork_session(h.context(&first).await).await;
    report.insert(
        "fork".into(),
        json!(format!(
            "{:?}",
            fork.as_ref().map(|state| &state.native_session_id)
        )),
    );

    h.driver.shutdown().await;
    let report = Value::Object(report);
    eprintln!(
        "REAL_CODEX_REPORT {}",
        serde_json::to_string_pretty(&report).unwrap()
    );
    let _ = std::fs::remove_dir_all(&root);

    // What Codex makes deterministic.
    assert!(report["models"]
        .as_array()
        .is_some_and(|models| !models.is_empty()));
    let approval = &report["approval"]["requests"][0];
    assert_eq!(approval["kind"], "command", "{report}");
    if let Some(decisions) = approval["availableDecisions"].as_array() {
        // Options follow Codex's list: Reject only when it allows `decline`.
        let offers_reject = approval["options"]
            .as_array()
            .unwrap()
            .contains(&json!("reject_once"));
        assert_eq!(
            offers_reject,
            decisions.contains(&json!("decline")),
            "{report}"
        );
    }
    assert_eq!(report["approval"]["fileWritten"], true, "{report}");
    assert_eq!(
        report["warnings"].as_array().map(Vec::len),
        Some(1),
        "{report}"
    );
    assert_eq!(
        report["editAndSteer"]["file"], "hello from todex.\n",
        "{report}"
    );
    assert_eq!(report["interrupt"]["interrupted"], true, "{report}");
    assert!(
        report["interrupt"]["result"]
            .as_str()
            .unwrap()
            .contains("cancelled: true"),
        "{report}"
    );
    assert_eq!(report["compact"], "Ok(Ok(()))", "{report}");
    assert!(
        report["fork"].as_str().unwrap().starts_with("Ok(Some("),
        "{report}"
    );
}
