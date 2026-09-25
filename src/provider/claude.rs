use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PermissionDecision, PermissionOutcome, ProviderCapabilities, ProviderDescriptor,
    ProviderDriver,
};

pub struct ClaudeDriver {
    binary: String,
}

fn claude_model_aliases() -> Vec<super::types::ProviderModelDescriptor> {
    ["default", "sonnet", "opus", "haiku"]
        .into_iter()
        .map(|id| super::types::ProviderModelDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: "Claude Code model alias".to_owned(),
            is_default: id == "default",
            supported_reasoning_efforts: ["low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            default_reasoning_effort: None,
            context_window: None,
            image_input: Some(true),
        })
        .collect()
}

impl ClaudeDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.claude_bin.clone(),
        }
    }
}

#[async_trait]
impl ProviderDriver for ClaudeDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::ClaudeCode,
            display_name: "Claude Code",
            available,
            unavailable_reason: (!available)
                .then(|| format!("executable '{}' was not found", self.binary)),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                permission_config: super::types::permission_config_capabilities(
                    ProviderKind::ClaudeCode,
                ),
                native_fork: true,
                native_compact: false,
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: true,
                model_selection: true,
                image_input: ProviderKind::ClaudeCode.supports_image_input(),
                image_input_mode: ImageInputMode::Always,
            },
            models: claude_model_aliases(),
        }
    }

    async fn discover_models(
        &self,
        _workspace: &Path,
    ) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        // The managed provider (live settings.json env) wins over the daemon's
        // own process environment so the catalog follows the active account.
        let Some(base) =
            crate::agent_providers::claude_live_env("ANTHROPIC_BASE_URL").or_else(|| {
                std::env::var("ANTHROPIC_BASE_URL")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
        else {
            return Ok(claude_model_aliases());
        };
        let url = format!("{}/v1/models", base.trim_end_matches('/'));
        let request = reqwest::Client::new().get(url);
        let request = match crate::agent_providers::claude_live_env("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| std::env::var("ANTHROPIC_AUTH_TOKEN").ok())
            .filter(|value| !value.trim().is_empty())
        {
            Some(token) => request.header("x-api-key", token),
            None => request,
        };
        let response = request
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|error| {
                AppError::ProviderUnavailable(format!("Claude model discovery failed: {error}"))
            })?;
        let payload: Value = response.json().await.map_err(|error| {
            AppError::ProviderUnavailable(format!("Claude model catalog invalid: {error}"))
        })?;
        let models = payload
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?.to_owned();
                Some(super::types::ProviderModelDescriptor {
                    display_name: item
                        .get("display_name")
                        .or_else(|| item.get("displayName"))
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    id,
                    description: "Claude gateway model".to_owned(),
                    is_default: false,
                    supported_reasoning_efforts: ["low", "medium", "high", "xhigh", "max"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    default_reasoning_effort: None,
                    context_window: item
                        .get("context_window")
                        .or_else(|| item.get("contextWindow"))
                        .and_then(Value::as_u64),
                    image_input: Some(true),
                })
            })
            .collect::<Vec<_>>();
        Ok(if models.is_empty() {
            claude_model_aliases()
        } else {
            models
        })
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        context: DriverContext,
        _launch_permit: WorkspaceTrustPermit,
    ) -> Result<crate::conversation::ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let config_dir = crate::agent_providers::claude_config_dir();
        let source_path =
            claude_session_path_at(&config_dir, &context.manifest.workspace, &source).await;
        if !tokio::fs::try_exists(&source_path).await.unwrap_or(false) {
            return Err(AppError::Unsupported(
                "conversation has no Claude transcript to fork".to_owned(),
            ));
        }
        let forked = uuid::Uuid::new_v4().to_string();
        fork_claude_transcript(
            &source_path,
            &config_dir,
            &context.manifest.workspace,
            &forked,
        )
        .await?;
        let mut state = crate::conversation::ProviderState::new(ProviderKind::ClaudeCode);
        state.native_session_id = Some(forked);
        state.recoverable = true;
        Ok(state)
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let controls = super::types::resolve_execution_config(
            context.manifest.provider,
            prompt.permission_mode.as_deref(),
            prompt.work_mode.as_deref(),
            prompt.permission_profile.as_deref(),
            prompt.sandbox_mode.as_deref(),
            prompt.approval_policy.as_deref(),
        )?;
        let requested_session_id = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "-p".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--include-partial-messages".to_owned(),
            "--replay-user-messages".to_owned(),
            "--permission-prompts".to_owned(),
            "host".to_owned(),
            // Match the official Agent SDK transport: host alone does not
            // connect can_use_tool requests to the stream-json control channel.
            "--permission-prompt-tool".to_owned(),
            "stdio".to_owned(),
            "--permission-mode".to_owned(),
            controls
                .provider_mode
                .expect("Claude permission mode validated"),
        ];
        if context.provider_state.native_session_id.is_some()
            || claude_session_exists(&context.manifest.workspace, &requested_session_id).await
        {
            // A turn that dies after Claude creates its transcript but before
            // the id reaches provider state (rate limit, crash, cancel) leaves
            // no recorded session: `--session-id` would then deadlock on
            // "Session ID is already in use", so resume the file instead.
            spec.args.push("--resume".to_owned());
        } else {
            spec.args.push("--session-id".to_owned());
        }
        spec.args.push(requested_session_id.clone());
        if let Some(model) = &prompt.model {
            spec.args.push("--model".to_owned());
            spec.args.push(model.clone());
        }
        if let Some(effort) = &prompt.reasoning_effort {
            spec.args.push("--effort".to_owned());
            spec.args.push(effort.clone());
        }

        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = run_claude_turn(
            &mut process,
            context,
            prompt,
            requested_session_id,
            &sink,
            &mut cancel,
        )
        .await;
        process.terminate().await;
        result
    }
}

/// Whether Claude already has a transcript for `session_id` in `workspace`.
/// Claude stores sessions at `<config>/projects/<dir>/<id>.jsonl` and rejects
/// `--session-id` for an id whose file exists, so a transcript on disk means
/// the next launch must `--resume` instead.
async fn claude_session_exists(workspace: &Path, session_id: &str) -> bool {
    claude_session_exists_at(
        &crate::agent_providers::claude_config_dir(),
        workspace,
        session_id,
    )
    .await
}

async fn claude_session_exists_at(config_dir: &Path, workspace: &Path, session_id: &str) -> bool {
    let file = claude_session_path_at(config_dir, workspace, session_id).await;
    tokio::fs::try_exists(file).await.unwrap_or(false)
}

async fn claude_session_path_at(config_dir: &Path, workspace: &Path, session_id: &str) -> PathBuf {
    let workspace = tokio::fs::canonicalize(workspace)
        .await
        .unwrap_or_else(|_| workspace.to_path_buf());
    config_dir
        .join("projects")
        .join(claude_project_dir_name(&workspace))
        .join(format!("{session_id}.jsonl"))
}

/// Fork a Claude session by copying its transcript under a fresh session id:
/// every `sessionId` field is rewritten so `--resume <id>` loads the copy as
/// its own session. `claude --resume --fork-session` achieves the same result
/// but only by spending a turn on a throwaway prompt; the transcript rewrite
/// keeps the fork free of side effects.
async fn fork_claude_transcript(
    source_path: &Path,
    config_dir: &Path,
    workspace: &Path,
    forked_id: &str,
) -> Result<(), AppError> {
    let metadata = tokio::fs::metadata(source_path).await?;
    let raw = tokio::fs::read(source_path).await?;
    let text = String::from_utf8(raw)
        .map_err(|_| AppError::InvalidRequest("Claude transcript is not UTF-8".to_owned()))?;
    let mut lines = Vec::with_capacity(text.lines().count());
    for line in text.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                if let Some(object) = value.as_object_mut() {
                    if object.get("sessionId").and_then(Value::as_str).is_some() {
                        object.insert("sessionId".to_owned(), json!(forked_id));
                    }
                }
                lines.push(serde_json::to_string(&value)?);
            }
            // Keep an unparseable line verbatim rather than corrupting the copy.
            Err(_) => lines.push(line.to_owned()),
        }
    }
    let destination = claude_session_path_at(config_dir, workspace, forked_id).await;
    if tokio::fs::try_exists(&destination).await.unwrap_or(false) {
        return Err(AppError::Conflict(
            "Claude fork destination already exists".to_owned(),
        ));
    }
    tokio::fs::write(&destination, format!("{}\n", lines.join("\n"))).await?;
    tokio::fs::set_permissions(&destination, metadata.permissions()).await?;
    Ok(())
}

/// The per-project directory name Claude Code derives from its cwd:
/// `cwd.replace(/[^a-zA-Z0-9]/g, "-")` over UTF-16 code units, and when that
/// exceeds 200 characters, `sanitized[..200] + "-" + abs(javaHash(cwd))` in
/// base36 (`NY`/`Le`/`gT` in the bundled CLI).
fn claude_project_dir_name(workspace: &Path) -> String {
    const MAX_PROJECT_DIR_CHARS: usize = 200;
    let raw = workspace.as_os_str().to_string_lossy();
    let sanitized: String = raw
        .encode_utf16()
        .map(|unit| {
            match u8::try_from(unit)
                .ok()
                .filter(|u| u.is_ascii_alphanumeric())
            {
                Some(u) => u as char,
                None => '-',
            }
        })
        .collect();
    if sanitized.len() <= MAX_PROJECT_DIR_CHARS {
        return sanitized;
    }
    let hash = raw.encode_utf16().fold(0_i32, |acc, unit| {
        acc.wrapping_mul(31).wrapping_add(unit as i32)
    });
    format!(
        "{}-{}",
        &sanitized[..MAX_PROJECT_DIR_CHARS],
        base36(hash.unsigned_abs() as u64)
    )
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.iter().rev().map(|&b| b as char).collect()
}

fn claude_user_content(prompt: &DriverPrompt) -> Value {
    let images = prompt
        .content
        .iter()
        .filter_map(|content| match content {
            super::types::DriverPromptContent::Image {
                data, mime_type, ..
            } => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                },
            })),
            super::types::DriverPromptContent::File { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Value::String(prompt.text.clone());
    }

    let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.text.is_empty()));
    if !prompt.text.is_empty() {
        content.push(json!({ "type": "text", "text": prompt.text }));
    }
    content.extend(images);
    Value::Array(content)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::path::Path;

    use super::{
        claude_model_aliases, claude_question_details, claude_question_response,
        claude_user_content, handle_stream_event, BackgroundTasks, ClaudeSubagents,
        ClaudeToolCalls,
    };
    use crate::conversation::{
        ConversationEventHub, ConversationManifest, ConversationStore, ProviderKind,
    };
    use crate::provider::types::{DriverEventSink, PermissionBroker};
    use crate::provider::types::{
        DriverPrompt, DriverPromptContent, PermissionDecision, PermissionOutcome,
    };

    #[tokio::test]
    async fn stream_text_merges_per_content_block_and_keeps_tool_json_fragments() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-deltas-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = ConversationStore::new(root.join("data")).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::ClaudeCode,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let sink = DriverEventSink::new(
            store.clone(),
            ConversationEventHub::default(),
            PermissionBroker::default(),
            &manifest.id,
        )
        .with_turn_id("turn-1");
        let mut tools = ClaudeToolCalls::default();
        let delta = |index: u64, delta: serde_json::Value| {
            json!({"type":"stream_event","event":{
                "type":"content_block_delta","index":index,"delta":delta
            }})
        };
        for message in [
            delta(0, json!({"type":"thinking_delta","thinking":"let me "})),
            delta(0, json!({"type":"thinking_delta","thinking":"see"})),
            delta(1, json!({"type":"text_delta","text":"Hel"})),
            delta(1, json!({"type":"text_delta","text":"lo"})),
            delta(2, json!({"type":"text_delta","text":" there"})),
            delta(
                3,
                json!({"type":"input_json_delta","partial_json":"{\"a\""}),
            ),
            delta(3, json!({"type":"input_json_delta","partial_json":":1}"})),
        ] {
            handle_stream_event(&message, "turn-1", &mut tools, &sink)
                .await
                .unwrap();
        }
        sink.emit("message.completed", json!({"provider":"claude-code"}))
            .await
            .unwrap();

        let history = store.complete_history(&manifest.id).await.unwrap();
        let journalled: Vec<_> = history
            .iter()
            .map(|event| (event.event_type.as_str(), event.payload["delta"].clone()))
            .collect();
        assert_eq!(
            journalled,
            [
                (
                    "thought.delta",
                    json!({"type":"thinking_delta","thinking":"let me see"})
                ),
                ("message.delta", json!({"type":"text_delta","text":"Hello"})),
                (
                    "message.delta",
                    json!({"type":"text_delta","text":" there"})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":"{\"a\""})
                ),
                (
                    "message.delta",
                    json!({"type":"input_json_delta","partial_json":":1}"})
                ),
                ("message.completed", serde_json::Value::Null),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn ask_user_question_input() -> serde_json::Value {
        json!({
            "questions": [
                {
                    "question": "Which file?",
                    "header": "Target",
                    "options": [
                        { "label": "web", "description": "TodeX_web/AGENTS.md" },
                        { "label": "desktop", "description": "TodeX_desktop/AGENTS.md" }
                    ],
                    "multiSelect": false
                },
                {
                    "question": "Which steps?",
                    "header": "Steps",
                    "options": [{ "label": "commit" }, { "label": "push" }],
                    "multiSelect": true
                }
            ]
        })
    }

    #[test]
    fn tool_snapshots_accumulate_name_input_and_result_under_one_block() {
        let mut tools = ClaudeToolCalls::default();
        let id = tools
            .record(&json!({ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} }))
            .unwrap();
        let started = tools.payload(&id, "turn-1", "started");
        assert_eq!(started["toolName"], "Bash");
        assert_eq!(started["block"]["id"], "toolu_1");
        assert_eq!(started["block"]["category"], "tool");

        tools.record(&json!({
            "type": "tool_use", "id": "toolu_1", "name": "Bash",
            "input": { "command": "echo ok" }
        }));
        // A late empty-input block never erases the complete arguments.
        tools.record(&json!({ "type": "tool_use", "id": "toolu_1", "input": {} }));
        assert_eq!(
            tools.payload(&id, "turn-1", "delta")["arguments"]["command"],
            "echo ok"
        );

        tools.complete(&json!({
            "type": "tool_result", "tool_use_id": "toolu_1",
            "content": [{ "type": "text", "text": "ok" }], "is_error": false
        }));
        let completed = tools.payload(&id, "turn-1", "completed");
        assert_eq!(completed["toolName"], "Bash");
        assert_eq!(completed["arguments"]["command"], "echo ok");
        assert_eq!(completed["result"], "ok");
        assert_eq!(completed["isError"], false);
        assert_eq!(completed["block"]["phase"], "completed");
    }

    #[test]
    fn ask_user_question_maps_to_the_shared_user_input_shape() {
        let details = claude_question_details(&ask_user_question_input()).unwrap();
        assert_eq!(details["questions"][0]["id"], "q0");
        assert_eq!(details["questions"][0]["question"], "Which file?");
        assert_eq!(details["questions"][0]["options"][1]["label"], "desktop");
        assert_eq!(details["questions"][1]["multiSelect"], true);
        assert_eq!(details["questions"][0]["isOther"], true);
        assert!(claude_question_details(&json!({ "questions": [] })).is_none());
        assert!(claude_question_details(&json!({ "questions": [{ "header": "x" }] })).is_none());
    }

    #[test]
    fn ask_user_question_answers_are_keyed_by_question_text() {
        let input = ask_user_question_input();
        let answered = claude_question_response(
            &input,
            &PermissionDecision {
                outcome: PermissionOutcome::Answer,
                option_id: Some("answer".to_owned()),
                data: Some(json!({ "answers": {
                    "q0": { "answers": ["web"] },
                    "q1": { "answers": ["commit", "push"] }
                } })),
            },
        );
        assert_eq!(answered["behavior"], "allow");
        assert_eq!(answered["updatedInput"]["questions"], input["questions"]);
        assert_eq!(
            answered["updatedInput"]["answers"],
            json!({ "Which file?": "web", "Which steps?": "commit, push" })
        );

        let skipped = claude_question_response(
            &input,
            &PermissionDecision {
                outcome: PermissionOutcome::RejectOnce,
                option_id: Some("reject_once".to_owned()),
                data: None,
            },
        );
        assert_eq!(skipped["behavior"], "deny");
    }

    #[test]
    fn project_dir_name_matches_claude_transcript_layout() {
        assert_eq!(
            super::claude_project_dir_name(Path::new("/Volumes/BIGDISK/github/Nitrous")),
            "-Volumes-BIGDISK-github-Nitrous"
        );
        // `.`, `_`, `-` are all non-alphanumeric in Claude's mapping.
        assert_eq!(
            super::claude_project_dir_name(Path::new("/Users/a.b_c/d-e")),
            "-Users-a-b-c-d-e"
        );
    }

    #[test]
    fn project_dir_name_truncates_long_paths_with_hash_suffix() {
        let long = format!("/{}", "a".repeat(300));
        let name = super::claude_project_dir_name(Path::new(&long));
        let (prefix, hash) = name.rsplit_once('-').unwrap();
        assert_eq!(prefix.len(), 200);
        // Pinned against Claude Code's `NY`/`Le` (Java string hash, base36).
        assert_eq!(hash, "vtkmfl");
        assert_eq!(
            super::claude_project_dir_name(Path::new(&long)),
            name,
            "hash suffix must be deterministic"
        );
    }

    #[tokio::test]
    async fn session_exists_detects_transcript_under_project_dir() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-session-lookup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("ws");
        let session = "b88ca64f-3ff5-46a3-989b-ff8188fb2d8d";
        let transcript = root
            .join("projects")
            .join(super::claude_project_dir_name(&workspace))
            .join(format!("{session}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}").unwrap();

        assert!(super::claude_session_exists_at(&root, &workspace, session).await);
        assert!(!super::claude_session_exists_at(&root, &workspace, "missing").await);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn transcript_fork_rewrites_session_ids_and_keeps_unparseable_lines() {
        let root = std::env::temp_dir().join(format!(
            "todex-claude-fork-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("ws");
        let source = "05811362-61b5-4b88-adec-0c75b9603987";
        let source_path = super::claude_session_path_at(&root, &workspace, source).await;
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        std::fs::write(
            &source_path,
            format!(
                concat!(
                    "{{\"type\":\"user\",\"sessionId\":\"{0}\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
                    "{{\"type\":\"assistant\",\"sessionId\":\"{0}\",\"uuid\":\"u2\",\"parentUuid\":\"u1\"}}\n",
                    "{{\"type\":\"summary\",\"summary\":\"half\"}}\n",
                    "not-a-json-line\n"
                ),
                source
            ),
        )
        .unwrap();

        let forked = "3d6e6184-90a2-403c-9b10-8bcdfc46cbbe";
        super::fork_claude_transcript(&source_path, &root, &workspace, forked)
            .await
            .unwrap();

        let forked_path = super::claude_session_path_at(&root, &workspace, forked).await;
        let copied = std::fs::read_to_string(&forked_path).unwrap();
        let lines: Vec<&str> = copied.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines[..2] {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["sessionId"], json!(forked));
        }
        // Lines without a sessionId keep their payload; unparseable lines pass through.
        let summary: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(summary["summary"], "half");
        assert_eq!(lines[3], "not-a-json-line");
        let user: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(user["message"]["content"], "hi");

        // The source transcript is untouched and a second fork to the same id conflicts.
        let original = std::fs::read_to_string(&source_path).unwrap();
        assert!(original.contains(source));
        assert!(matches!(
            super::fork_claude_transcript(&source_path, &root, &workspace, forked).await,
            Err(crate::error::AppError::Conflict(_))
        ));
        assert!(
            super::fork_claude_transcript(
                &root.join("missing.jsonl"),
                &root,
                &workspace,
                "another-fork"
            )
            .await
            .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn built_in_model_aliases_are_selectable_without_gateway_discovery() {
        let models = claude_model_aliases();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "sonnet", "opus", "haiku"]
        );
        assert!(models[0].is_default);
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn background_tasks_track_live_set_until_notifications() {
        let mut tasks = BackgroundTasks::default();
        assert!(tasks.is_empty());

        tasks.apply(&json!({ "subtype": "task_started", "task_id": "a1" }));
        tasks
            .apply(&json!({ "subtype": "task_started", "task_id": "b2", "is_backgrounded": true }));
        assert!(!tasks.is_empty());

        // Foreground task starts are not background work that outlives a turn.
        tasks.apply(
            &json!({ "subtype": "task_started", "task_id": "fg", "is_backgrounded": false }),
        );
        tasks.apply(
            &json!({ "subtype": "task_notification", "task_id": "a1", "status": "completed" }),
        );
        assert_eq!(tasks.ids().len(), 1);

        // background_tasks_changed replaces the whole live set.
        tasks.apply(&json!({
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "c3" }, { "task_id": "d4" }]
        }));
        let mut ids = tasks
            .ids()
            .into_iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["c3", "d4"]);

        tasks.apply(&json!({ "subtype": "background_tasks_changed", "tasks": [] }));
        assert!(tasks.is_empty());
    }

    #[test]
    fn task_tool_call_opens_a_subagent_and_task_frames_update_it() {
        let mut subagents = ClaudeSubagents::default();

        let started = subagents
            .start_from_tool(
                &json!({
                    "type": "tool_use", "id": "toolu_1", "name": "Task",
                    "input": { "description": "Map contracts", "subagent_type": "Explore",
                        "prompt": "inspect the workspace" }
                }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(started["subagentId"], "toolu_1");
        assert_eq!(started["providerItemId"], "toolu_1");
        assert_eq!(started["agentKind"], "Explore");
        assert_eq!(started["task"], "inspect the workspace");
        assert!(subagents
            .start_from_tool(
                &json!({ "type": "tool_use", "id": "toolu_1", "name": "Task", "input": {} }),
                "turn-1",
            )
            .is_none());

        // task_started only links the task id back to the surfaced run.
        assert!(subagents
            .system_event(
                &json!({ "subtype": "task_started", "task_id": "agent-9",
                    "task_type": "local_agent", "tool_use_id": "toolu_1" }),
                "turn-1",
            )
            .is_none());

        let progress = subagents
            .system_event(
                &json!({ "subtype": "task_progress", "task_id": "agent-9",
                    "status": "running" }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(progress.0, "subagent.updated");
        assert_eq!(progress.1["subagentId"], "toolu_1");

        let done = subagents
            .system_event(
                &json!({ "subtype": "task_notification", "task_id": "agent-9",
                    "status": "completed", "summary": "mapped",
                    "agent_id": "aabe", "output_file": "/tmp/out.md" }),
                "turn-1",
            )
            .unwrap();
        assert_eq!(done.0, "subagent.completed");
        assert_eq!(done.1["subagentId"], "toolu_1");
        assert_eq!(done.1["providerItemId"], "toolu_1");
        assert_eq!(done.1["agentId"], "aabe");
        assert_eq!(done.1["result"], "mapped");
        assert_eq!(done.1["metadata"]["outputFile"], "/tmp/out.md");

        // A trailing tool_result cannot reopen a notified run.
        assert!(subagents
            .finish_from_tool("toolu_1", &json!({}), &json!("late"), false, "turn-1")
            .is_none());
    }

    #[test]
    fn foreground_agent_finishes_with_its_tool_result() {
        let mut subagents = ClaudeSubagents::default();
        subagents.start_from_tool(
            &json!({ "type": "tool_use", "id": "toolu_2", "name": "Agent",
                "input": { "description": "run tests" } }),
            "turn-1",
        );

        let done = subagents
            .finish_from_tool("toolu_2", &json!({}), &json!("all green"), false, "turn-1")
            .unwrap();
        assert_eq!(done.0, "subagent.completed");
        assert_eq!(done.1["result"], "all green");
    }

    #[test]
    fn async_tool_result_keeps_the_subagent_running() {
        let mut subagents = ClaudeSubagents::default();
        subagents.start_from_tool(
            &json!({ "type": "tool_use", "id": "toolu_3", "name": "Task",
                "input": { "description": "background sweep" } }),
            "turn-1",
        );

        let update = subagents
            .finish_from_tool(
                "toolu_3",
                &json!({ "tool_use_result": {
                    "isAsync": true, "status": "async_launched", "agentId": "a1" } }),
                &json!("Async agent launched"),
                false,
                "turn-1",
            )
            .unwrap();
        assert_eq!(update.0, "subagent.updated");
        assert_eq!(update.1["agentId"], "a1");
        assert_eq!(update.1["status"], "running");

        // Non-agent task kinds never enter the subagent surface.
        assert!(subagents
            .system_event(
                &json!({ "subtype": "task_started", "task_id": "sh-1",
                    "task_type": "local_shell", "tool_use_id": "toolu_sh" }),
                "turn-1",
            )
            .is_none());
    }

    #[test]
    fn text_only_prompt_keeps_the_existing_string_shape() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };

        assert_eq!(claude_user_content(&prompt), json!("hello"));
    }

    #[test]
    fn image_prompt_uses_claude_streaming_content_blocks() {
        let prompt = DriverPrompt {
            turn_id: "turn-2".to_owned(),
            text: "describe this".to_owned(),
            content: vec![DriverPromptContent::Image {
                path: None,
                data: "cG5n".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };

        assert_eq!(
            claude_user_content(&prompt),
            json!([
                { "type": "text", "text": "describe this" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "cG5n",
                    },
                },
            ])
        );
    }
}

// Protocol reference: anthropics/claude-agent-sdk-python, _internal/query.py.
// Use the same initialize exchange as the official Agent SDK before sending
// a user turn. A rejected/unsupported control channel must fail before tools run.
async fn initialize_claude(
    process: &mut JsonLineProcess,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    process
        .send(&json!({
            "type": "control_request",
            "request_id": "todex-initialize",
            "request": { "subtype": "initialize", "hooks": null }
        }))
        .await?;
    let initialize = async {
        loop {
            let Some(message) = process.read().await? else {
                return Err(provider_exit_error(
                    process,
                    "Claude Code closed during initialization",
                )
                .await);
            };
            if message.get("type").and_then(Value::as_str) == Some("result")
                && message.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                return Err(AppError::ProviderUnavailable(format!(
                    "Claude Code initialization failed: {}",
                    message
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or("provider rejected startup")
                )));
            }
            if message.get("type").and_then(Value::as_str) == Some("control_response")
                && message
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    == Some("todex-initialize")
            {
                return if message.pointer("/response/subtype").and_then(Value::as_str)
                    == Some("success")
                {
                    Ok(())
                } else {
                    Err(AppError::ProviderUnavailable(format!(
                        "Claude Code initialization rejected: {}",
                        message
                            .pointer("/response/error")
                            .and_then(Value::as_str)
                            .unwrap_or("unsupported control protocol")
                    )))
                };
            }
        }
    };
    tokio::select! {
        result = tokio::time::timeout(super::process::control_timeout()?, initialize) => {
            result.map_err(|_| AppError::ProviderUnavailable("Claude Code initialization timed out".to_owned()))?
        }
        _ = cancel.changed() => Err(AppError::InvalidRequest("Claude Code initialization cancelled".to_owned())),
    }
}

async fn run_claude_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    requested_session_id: String,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    initialize_claude(process, cancel).await?;
    // The transcript exists once initialize succeeds; persist its id right
    // away so a failed or cancelled turn still resumes instead of hitting
    // "Session ID is already in use" on the next launch.
    if context.provider_state.native_session_id.as_deref() != Some(requested_session_id.as_str()) {
        let mut provider_state = context.provider_state.clone();
        provider_state.native_session_id = Some(requested_session_id.clone());
        provider_state.recoverable = true;
        sink.save_provider_state(provider_state).await?;
    }
    let expected_mode = super::types::resolve_execution_config(
        context.manifest.provider,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?
    .provider_mode;
    let user_message = json!({
        "type": "user",
        "session_id": requested_session_id,
        "parent_tool_use_id": null,
        "message": {
            "role": "user",
            "content": claude_user_content(&prompt),
        }
    });
    process.send(&user_message).await?;

    let mut saw_output = false;
    let mut tools = ClaudeToolCalls::default();
    let mut background_tasks = BackgroundTasks::default();
    let mut subagents = ClaudeSubagents::default();
    // A `result` with zero model turns means the invocation ended without an
    // API call — a task notification queued during resume consumes the prompt
    // without answering it. The process stays alive on the open stream, so
    // resend the prompt instead of failing the turn.
    let mut empty_results = 0_u32;
    loop {
        let message = tokio::select! {
            message = process.read() => message?,
                changed = cancel.changed() => {
                    let _ = changed;
                    return Ok(DriverTurnResult {
                    native_session_id: Some(requested_session_id.clone()),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Claude Code closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("result") => {
                if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
                    sink.emit("usage.updated", json!({ "provider": "claude-code", "turnId": prompt.turn_id, "source": "provider", "scope": "turn", "usage": usage, "costUsd": message.get("total_cost_usd") })).await?;
                }
                let native_session_id = message
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&requested_session_id)
                    .to_owned();
                let is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    return Err(AppError::ProviderUnavailable(
                        message
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or("Claude Code returned an error")
                            .chars()
                            .take(1000)
                            .collect(),
                    ));
                }
                if message.get("num_turns").and_then(Value::as_u64) == Some(0)
                    && empty_results < MAX_EMPTY_RESULT_RESENDS
                {
                    empty_results += 1;
                    process.send(&user_message).await?;
                    continue;
                }
                // `result` ends the model turn, not the process: while live
                // background tasks remain, Claude keeps the stream open,
                // delivers task_notification, and runs a follow-up turn that
                // ends in another `result`. Terminating here kills those
                // subagents mid-flight.
                if !background_tasks.is_empty() {
                    sink.emit(
                        "provider.event",
                        json!({
                            "provider": "claude-code",
                            "providerMethod": "background_tasks_pending",
                            "metadata": { "taskIds": background_tasks.ids() },
                        }),
                    )
                    .await?;
                    continue;
                }
                if !saw_output {
                    return Err(AppError::ProviderUnavailable(
                        "Claude Code completed without producing output; please retry".to_owned(),
                    ));
                }
                let mut provider_state = context.provider_state;
                provider_state.native_session_id = Some(native_session_id.clone());
                provider_state.recoverable = true;
                provider_state.last_error = None;
                sink.save_provider_state(provider_state).await?;
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: message
                        .get("subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .to_owned(),
                    cancelled: false,
                });
            }
            Some("stream_event") => {
                saw_output = true;
                handle_stream_event(&message, &prompt.turn_id, &mut tools, sink).await?;
            }
            Some("assistant") => {
                saw_output = true;
                sink.emit(
                    "message.completed",
                    json!({ "provider": "claude-code", "message": message.get("message") }),
                )
                .await?;
                // The streamed `tool_use` start carries an empty input; the
                // complete assistant message is the first with arguments.
                for block in content_blocks(&message, "tool_use") {
                    if let Some(id) = tools.record(block) {
                        if ClaudeSubagents::is_agent_tool(block.get("name")) {
                            if let Some(payload) = subagents.start_from_tool(block, &prompt.turn_id)
                            {
                                sink.emit("subagent.started", payload).await?;
                            }
                        }
                        sink.emit("tool.updated", tools.payload(&id, &prompt.turn_id, "delta"))
                            .await?;
                    }
                }
            }
            Some("user") => {
                for block in content_blocks(&message, "tool_result") {
                    if let Some(id) = tools.complete(block) {
                        let payload = tools.payload(&id, &prompt.turn_id, "completed");
                        sink.emit("tool.completed", payload.clone()).await?;
                        if let Some((event, subagent)) = subagents.finish_from_tool(
                            &id,
                            &message,
                            payload.get("result").unwrap_or(&Value::Null),
                            payload
                                .get("isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            &prompt.turn_id,
                        ) {
                            sink.emit(event, subagent).await?;
                        }
                    }
                }
            }
            Some("tool_progress") => {
                saw_output = true;
                if let Some(id) = message.get("tool_use_id").and_then(Value::as_str) {
                    tools.name_if_unknown(id, message.get("tool_name"));
                    sink.emit("tool.updated", tools.payload(id, &prompt.turn_id, "delta"))
                        .await?;
                }
            }
            Some("control_request") => {
                handle_control_request(process, message, sink, cancel).await?;
            }
            Some("system") => {
                background_tasks.apply(&message);
                if let Some((event, payload)) = subagents.system_event(&message, &prompt.turn_id) {
                    sink.emit(event, payload).await?;
                }
                if message.get("subtype").and_then(Value::as_str) == Some("init") {
                    if let Some(actual) = message.get("permissionMode").and_then(Value::as_str) {
                        if expected_mode.as_deref() != Some(actual) {
                            return Err(AppError::ProviderUnavailable(format!(
                                "Claude Code did not activate requested permission mode '{}'; reported '{}'. Check model, account and organization permissions.",
                                expected_mode.as_deref().unwrap_or("default"), actual
                            )));
                        }
                    }
                }
                sink.emit(
                    "provider.event",
                    json!({
                        "provider": "claude-code",
                        "providerMethod": message.get("subtype"),
                        "metadata": {
                            "sessionId": message.get("session_id"),
                            "model": message.get("model"),
                            "permissionMode": message.get("permissionMode"),
                            "taskId": message.get("task_id"),
                            "taskType": message.get("task_type"),
                            "taskStatus": message.get("status"),
                            "description": message.get("description"),
                            "toolUseId": message.get("tool_use_id"),
                        }
                    }),
                )
                .await?;
            }
            Some("control_response") => {}
            Some(event_type) => {
                sink.emit(
                    "provider.event",
                    json!({ "provider": "claude-code", "providerMethod": event_type }),
                )
                .await?;
            }
            None => {}
        }
    }
}

fn content_blocks<'a>(message: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    message
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |block| block.get("type").and_then(Value::as_str) == Some(kind))
}

/// Resend attempts allowed when a `result` reports zero model turns. Several
/// queued notifications can each consume an invocation, so the cap is above
/// one but still bounded.
const MAX_EMPTY_RESULT_RESENDS: u32 = 3;

/// Live background-task set, mirroring the Agent SDK: `task_started` adds
/// (unless explicitly foreground), `task_notification` removes, and
/// `background_tasks_changed` replaces the whole set — its `tasks` payload is
/// "every live background task after the change".
#[derive(Default)]
struct BackgroundTasks(HashSet<String>);

impl BackgroundTasks {
    fn apply(&mut self, message: &Value) {
        match message.get("subtype").and_then(Value::as_str) {
            Some("task_started") => {
                if message.get("is_backgrounded").and_then(Value::as_bool) == Some(false) {
                    return;
                }
                if let Some(id) = message.get("task_id").and_then(Value::as_str) {
                    self.0.insert(id.to_owned());
                }
            }
            Some("task_notification") => {
                if let Some(id) = message.get("task_id").and_then(Value::as_str) {
                    self.0.remove(id);
                }
            }
            Some("background_tasks_changed") => {
                self.0.clear();
                if let Some(tasks) = message.get("tasks").and_then(Value::as_array) {
                    self.0.extend(tasks.iter().filter_map(|task| {
                        task.get("task_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    }));
                }
            }
            _ => {}
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn ids(&self) -> Vec<Value> {
        self.0.iter().map(|id| Value::String(id.clone())).collect()
    }
}

/// Subagent runs keyed by the surfaced `subagentId`: the spawning Task/Agent
/// tool_use id when the run came from a tool call, otherwise the provider
/// task_id. `by_task` links background task frames (keyed by task_id) back to
/// that surfaced id so progress and terminal notifications update one entry.
#[derive(Default)]
struct ClaudeSubagents {
    /// subagentId → tool_use_id of the spawning Task/Agent call.
    items: HashMap<String, String>,
    /// task_id → subagentId for every task classified as a subagent.
    by_task: HashMap<String, String>,
    /// subagentIds that already emitted a terminal event.
    finished: HashSet<String>,
}

impl ClaudeSubagents {
    fn is_agent_tool(name: Option<&Value>) -> bool {
        matches!(name.and_then(Value::as_str), Some("Task" | "Agent"))
    }

    /// Background shells and monitors ride the same task frames; only agent
    /// kinds belong on the subagent surface.
    fn is_agent_task_kind(kind: Option<&Value>) -> bool {
        kind.and_then(Value::as_str)
            .is_some_and(|kind| kind.contains("agent"))
    }

    /// A Task/Agent `tool_use` opens a subagent run even when no task frame
    /// ever arrives — foreground agents only report through their tool result.
    fn start_from_tool(&mut self, block: &Value, turn_id: &str) -> Option<Value> {
        let tool_id = block.get("id").and_then(Value::as_str)?.to_owned();
        if self.items.contains_key(&tool_id) {
            return None;
        }
        self.items.insert(tool_id.clone(), tool_id.clone());
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        Some(json!({
            "provider": "claude-code",
            "source": "provider",
            "turnId": turn_id,
            "subagentId": tool_id,
            "providerItemId": tool_id,
            "agentKind": input.get("subagent_type"),
            "title": input.get("description").or_else(|| input.get("subagent_type")),
            "task": input.get("prompt").or_else(|| input.get("description")),
            "status": "running",
        }))
    }

    /// The tool_result of a Task/Agent call ends the run for foreground
    /// agents. Background launches report `async_launched` and finish through
    /// their task_notification instead.
    fn finish_from_tool(
        &mut self,
        tool_id: &str,
        message: &Value,
        result: &Value,
        is_error: bool,
        turn_id: &str,
    ) -> Option<(&'static str, Value)> {
        if !self.items.contains_key(tool_id) || self.finished.contains(tool_id) {
            return None;
        }
        let tool_use_result = message
            .get("tool_use_result")
            .cloned()
            .unwrap_or(Value::Null);
        let async_launch = tool_use_result
            .get("isAsync")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || tool_use_result
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status == "async_launched");
        if async_launch {
            return Some((
                "subagent.updated",
                json!({
                    "provider": "claude-code",
                    "source": "provider",
                    "turnId": turn_id,
                    "subagentId": tool_id,
                    "providerItemId": tool_id,
                    "agentId": tool_use_result.get("agentId"),
                    "status": "running",
                    "metadata": { "outputFile": tool_use_result.get("outputFile") },
                }),
            ));
        }
        self.finished.insert(tool_id.to_owned());
        Some((
            if is_error {
                "subagent.failed"
            } else {
                "subagent.completed"
            },
            json!({
                "provider": "claude-code",
                "source": "provider",
                "turnId": turn_id,
                "subagentId": tool_id,
                "providerItemId": tool_id,
                "status": if is_error { "failed" } else { "completed" },
                "result": result,
                "error": is_error.then(|| result.clone()),
            }),
        ))
    }

    /// `task_started`, `task_progress` and `task_notification` frames carry the
    /// subagent lifecycle for provider-side tasks.
    fn system_event(&mut self, message: &Value, turn_id: &str) -> Option<(&'static str, Value)> {
        let task_id = message.get("task_id").and_then(Value::as_str)?;
        match message.get("subtype").and_then(Value::as_str) {
            Some("task_started") => {
                let tool_use_id = message.get("tool_use_id").and_then(Value::as_str);
                let kind = message.get("task_type");
                // A linked Task call already opened the run from its tool_use;
                // record the task_id link so later task frames find it.
                if let Some(subagent_id) = tool_use_id.and_then(|id| self.items.get(id)).cloned() {
                    self.by_task.insert(task_id.to_owned(), subagent_id);
                    return None;
                }
                if !Self::is_agent_task_kind(kind) {
                    return None;
                }
                self.by_task.insert(task_id.to_owned(), task_id.to_owned());
                if let Some(tool_use_id) = tool_use_id {
                    self.items
                        .insert(task_id.to_owned(), tool_use_id.to_owned());
                }
                Some((
                    "subagent.started",
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": task_id,
                        "providerItemId": tool_use_id,
                        "agentKind": kind,
                        "agentId": message.get("agent_id"),
                        "title": message
                            .get("description")
                            .or(kind)
                            .filter(|title| title.is_string()),
                        "task": message.get("description"),
                        "status": "running",
                    }),
                ))
            }
            Some("task_progress") => {
                let subagent_id = self.by_task.get(task_id)?;
                if self.finished.contains(subagent_id) {
                    return None;
                }
                Some((
                    "subagent.updated",
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": subagent_id,
                        "status": message.get("status"),
                        "metadata": {
                            "taskId": task_id,
                            "description": message.get("description"),
                            "toolName": message.get("tool_name"),
                            "usage": message.get("usage"),
                        },
                    }),
                ))
            }
            Some("task_notification") => {
                let subagent_id = self.by_task.get(task_id)?.clone();
                if self.finished.contains(&subagent_id) {
                    return None;
                }
                self.finished.insert(subagent_id.clone());
                let status = message
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed");
                let event = match status {
                    "completed" => "subagent.completed",
                    "failed" => "subagent.failed",
                    _ => "subagent.cancelled",
                };
                Some((
                    event,
                    json!({
                        "provider": "claude-code",
                        "source": "provider",
                        "turnId": turn_id,
                        "subagentId": subagent_id,
                        "providerItemId": self.items.get(&subagent_id),
                        "agentId": message.get("agent_id"),
                        "status": status,
                        "result": message.get("summary"),
                        "error": (status != "completed")
                            .then(|| message.get("summary").cloned())
                            .flatten(),
                        "metadata": {
                            "taskId": task_id,
                            "outputFile": message.get("output_file"),
                            "stopCause": message.get("stop_cause"),
                            "snapshot": message.get("snapshot"),
                        },
                    }),
                ))
            }
            _ => None,
        }
    }
}

#[derive(Default)]
struct ClaudeToolCall {
    name: Value,
    input: Value,
    result: Option<Value>,
    is_error: Option<bool>,
}

/// Claude reports one tool call across several messages: the streamed
/// `tool_use` start (empty input), the complete assistant message, progress
/// ticks and the replayed `tool_result`. Clients replace a tool card with the
/// latest snapshot, so every event carries everything known about the call.
#[derive(Default)]
struct ClaudeToolCalls(HashMap<String, ClaudeToolCall>);

impl ClaudeToolCalls {
    /// Record a `tool_use` block, keeping earlier non-empty input when a
    /// later block (the stream start) has none.
    fn record(&mut self, block: &Value) -> Option<String> {
        let id = block.get("id").and_then(Value::as_str)?.to_owned();
        let call = self.0.entry(id.clone()).or_default();
        if let Some(name) = block.get("name").filter(|name| name.is_string()) {
            call.name = name.clone();
        }
        if let Some(input) = block
            .get("input")
            .filter(|input| input.as_object().is_some_and(|input| !input.is_empty()))
        {
            call.input = input.clone();
        }
        Some(id)
    }

    fn name_if_unknown(&mut self, id: &str, name: Option<&Value>) {
        let call = self.0.entry(id.to_owned()).or_default();
        if call.name.is_null() {
            if let Some(name) = name.filter(|name| name.is_string()) {
                call.name = name.clone();
            }
        }
    }

    fn complete(&mut self, block: &Value) -> Option<String> {
        let id = block.get("tool_use_id").and_then(Value::as_str)?.to_owned();
        let call = self.0.entry(id.clone()).or_default();
        call.result = Some(tool_result_text(block.get("content")));
        call.is_error = Some(
            block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        Some(id)
    }

    fn payload(&self, id: &str, turn_id: &str, phase: &str) -> Value {
        let call = self.0.get(id);
        json!({
            "provider": "claude-code",
            "toolCallId": id,
            "toolName": call.map_or(&Value::Null, |call| &call.name),
            "arguments": call.map_or(&Value::Null, |call| &call.input),
            "result": call.and_then(|call| call.result.as_ref()),
            "isError": call.and_then(|call| call.is_error),
            "block": {
                "category": "tool",
                "id": id,
                "turnId": turn_id,
                "phase": phase,
            },
        })
    }
}

/// `tool_result` content is a string or a list of content blocks; text blocks
/// are joined, anything else (images) is kept as-is.
fn tool_result_text(content: Option<&Value>) -> Value {
    match content {
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                Value::Array(blocks.clone())
            } else {
                Value::String(text)
            }
        }
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

async fn handle_stream_event(
    message: &Value,
    turn_id: &str,
    tools: &mut ClaudeToolCalls,
    sink: &DriverEventSink,
) -> Result<(), AppError> {
    let event = message.get("event").cloned().unwrap_or(Value::Null);
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta").cloned().unwrap_or(Value::Null);
            let delta_type = delta.get("type").and_then(Value::as_str);
            let event_type = if delta_type == Some("thinking_delta") {
                "thought.delta"
            } else {
                "message.delta"
            };
            let payload = json!({ "provider": "claude-code", "role": "assistant", "delta": delta });
            // Only text and thinking merge; tool-argument JSON and signatures
            // stay one event per fragment.
            let text: Option<&'static [&'static str]> = match delta_type {
                Some("text_delta") => Some(&["/delta/text"]),
                Some("thinking_delta") => Some(&["/delta/thinking"]),
                _ => None,
            };
            match text {
                Some(text) => {
                    // The payload omits the content-block index; keep blocks
                    // (and subagent streams) apart anyway.
                    let block = json!([event.get("index"), message.get("parent_tool_use_id")]);
                    sink.emit_delta(event_type, payload, text, Some(&block.to_string()))
                        .await?;
                }
                None => {
                    sink.emit(event_type, payload).await?;
                }
            }
        }
        "content_block_start" => {
            let content = event.get("content_block").cloned().unwrap_or(Value::Null);
            if content.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(id) = tools.record(&content) {
                    sink.emit("tool.started", tools.payload(&id, turn_id, "started"))
                        .await?;
                }
            }
        }
        "message_stop" | "message_start" | "content_block_stop" => {}
        _ => {
            sink.emit(
                "provider.event",
                json!({ "provider": "claude-code", "providerMethod": event_type }),
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_control_request(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let request_id = message
        .get("request_id")
        .or_else(|| message.pointer("/request/request_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::InvalidRequest("Claude control request is missing request_id".to_owned())
        })?;
    let request = message.get("request").cloned().unwrap_or(Value::Null);
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if subtype != "can_use_tool" {
        process
            .send(&json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": "control request is not supported by TodeX"
                }
            }))
            .await?;
        return Ok(());
    }

    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("operation");
    if tool_name == ASK_USER_QUESTION_TOOL {
        if let Some(details) = request.get("input").and_then(claude_question_details) {
            let decision = sink
                .request_permission(
                    request_id.to_owned(),
                    "user_input",
                    "Claude needs input",
                    details,
                    json!([
                        { "id": "answer", "kind": "answer", "name": "Answer" },
                        { "id": "reject_once", "kind": "reject_once", "name": "Skip" }
                    ]),
                    cancel,
                )
                .await?;
            let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
            return send_permission_response(
                process,
                request_id,
                claude_question_response(&input, &decision),
            )
            .await;
        }
    }

    let decision = sink
        .request_permission(
            request_id.to_owned(),
            "tool",
            format!("Allow Claude tool {tool_name}?"),
            request.clone(),
            json!([
                { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
                { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
            ]),
            cancel,
        )
        .await?;
    let response = match decision.outcome {
        PermissionOutcome::AllowOnce | PermissionOutcome::AllowAlways => {
            json!({
                "behavior": "allow",
                "updatedInput": request.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        }
        PermissionOutcome::RejectOnce
        | PermissionOutcome::RejectAlways
        | PermissionOutcome::Answer
        | PermissionOutcome::AbortTurn => json!({
            "behavior": "deny",
            "message": "User rejected this tool request",
        }),
    };
    send_permission_response(process, request_id, response).await
}

async fn send_permission_response(
    process: &mut JsonLineProcess,
    request_id: &str,
    response: Value,
) -> Result<(), AppError> {
    process
        .send(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            }
        }))
        .await
}

/// Claude's clarifying-question tool. It is answered rather than approved:
/// the tool reads the user's picks from `updatedInput.answers`, so allowing it
/// with the original input reads as "the user did not answer".
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

fn claude_question_id(index: usize) -> String {
    format!("q{index}")
}

/// Surface `AskUserQuestion` as the shared `user_input` request so every
/// client's question form applies. Returns `None` for malformed input, which
/// then falls back to a plain tool approval.
fn claude_question_details(input: &Value) -> Option<Value> {
    let questions = input.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let questions = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let text = question
                .get("question")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())?;
            Some(json!({
                "id": claude_question_id(index),
                "question": text,
                "header": question.get("header").and_then(Value::as_str),
                "options": question.get("options").filter(|options| options.is_array()),
                "multiSelect": question.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                // Claude accepts a free-text answer in place of the options.
                "isOther": true,
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(json!({ "questions": questions }))
}

/// Map `user_input` answers (validated by the broker to cover every question)
/// back to Claude's shape: keyed by question text, multi-select picks joined
/// with ", " as the SDK documents.
fn claude_question_response(input: &Value, decision: &PermissionDecision) -> Value {
    if !matches!(decision.outcome, PermissionOutcome::Answer) {
        return json!({
            "behavior": "deny",
            "message": "User declined to answer the question",
        });
    }
    let answers = decision.data.as_ref().and_then(|data| data.get("answers"));
    let mut mapped = Map::new();
    let questions = input.get("questions").and_then(Value::as_array);
    for (index, question) in questions.into_iter().flatten().enumerate() {
        let Some(text) = question.get("question").and_then(Value::as_str) else {
            continue;
        };
        let picks = answers
            .and_then(|answers| answers.get(claude_question_id(index)))
            .and_then(|answer| answer.get("answers"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        mapped.insert(text.to_owned(), Value::String(picks));
    }
    let mut updated = input.as_object().cloned().unwrap_or_default();
    updated.insert("answers".to_owned(), Value::Object(mapped));
    json!({ "behavior": "allow", "updatedInput": updated })
}
