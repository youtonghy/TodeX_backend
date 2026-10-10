use std::{fs, path::PathBuf};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::api_keys::{ApiKeyScopes, NewApiKey};
use crate::config::Config;
use crate::conversation::e2e_support::{all_bytes, contains};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    state: AppState,
    app: Router,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A daemon state whose Antigravity CLI is the stream-json fixture, with
/// no paired device and no history recipient at all: API key
/// conversations must not need one.
async fn fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("todex-api-{}", uuid::Uuid::new_v4().simple()));
    let workspace = root.join("workspaces/project");
    fs::create_dir_all(&workspace).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let workspace = fs::canonicalize(workspace).unwrap();
    let agy = root.join("agy-fixture");
    fs::write(
        &agy,
        include_str!("../../../tests/fixtures/antigravity_stream_fixture.py"),
    )
    .unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = Config {
        port: 0,
        data_dir: root.join("data"),
        workspace_roots: vec![root.join("workspaces")],
        ..Config::default()
    };
    config.agent.antigravity_bin = agy.to_string_lossy().into_owned();
    config.agent.provider_idle_timeout_minutes = 0;
    let state = AppState::new(config).await.unwrap();
    let app = api_router(state.clone()).layer(axum::extract::connect_info::MockConnectInfo(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
    ));
    Fixture {
        root,
        workspace,
        state,
        app,
    }
}

impl Fixture {
    fn key(&self, scopes: ApiKeyScopes) -> (crate::api_keys::ApiKeyRecord, String) {
        let (record, key) = self
            .state
            .api_keys
            .create(NewApiKey {
                name: "test".to_owned(),
                scopes,
                ..NewApiKey::default()
            })
            .unwrap();
        (record, key.to_string())
    }

    /// A key that may use the fixture workspace (listing it grants trust).
    fn workspace_key(&self) -> (crate::api_keys::ApiKeyRecord, String) {
        self.key(ApiKeyScopes {
            agents: None,
            workspaces: Some(vec![self.workspace.clone()]),
        })
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, uri, key, body, None).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn raw(
        &self,
        method: &str,
        uri: &str,
        key: Option<&str>,
        body: Option<Value>,
        accept: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            to_bytes(response.into_body(), 16 * 1024 * 1024),
        )
        .await
        .expect("the response ends")
        .unwrap();
        (status, bytes.to_vec())
    }
}

/// `(id, event, data)` of every SSE event in `body`.
fn sse_events(body: &[u8]) -> Vec<(Option<u64>, String, Value)> {
    String::from_utf8_lossy(body)
        .split("\n\n")
        .filter_map(|block| {
            let mut id = None;
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("id:") {
                    id = value.trim().parse().ok();
                } else if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push_str(value.trim_start());
                }
            }
            Some((
                id,
                event?,
                serde_json::from_str(&data).unwrap_or(Value::Null),
            ))
        })
        .collect()
}

#[tokio::test]
async fn keys_gate_every_route_but_health() {
    let fixture = fixture().await;
    let (status, body) = fixture.call("GET", "/api/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    for key in [None, Some("tdx_0000000000000000_nope"), Some("garbage")] {
        let (status, body) = fixture.call("GET", "/api/v1/me", key, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "UNAUTHENTICATED");
    }
    let (record, key) = fixture.workspace_key();
    let (status, body) = fixture.call("GET", "/api/v1/me", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], record.id);
    assert_eq!(body["approval"], "ask");
    assert!(body.get("secretHash").is_none());

    assert!(fixture.state.api_keys.revoke(&record.id).unwrap());
    let (status, _) = fixture.call("GET", "/api/v1/me", Some(&key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn scopes_and_workspace_trust_limit_what_a_key_reaches() {
    let fixture = fixture().await;
    let workspace = fixture.workspace.to_string_lossy().into_owned();
    let create = |agent: &str| json!({ "agent": agent, "workspace": workspace });

    let (_, codex_only) = fixture.key(ApiKeyScopes {
        agents: Some(vec![ProviderKind::Codex]),
        workspaces: Some(vec![fixture.workspace.clone()]),
    });
    let (status, body) = fixture
        .call("GET", "/api/v1/agents", Some(&codex_only), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "codex");
    let (status, _) = fixture
        .call(
            "POST",
            "/api/v1/conversations",
            Some(&codex_only),
            Some(create("antigravity")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Another directory under the root, outside the key's workspace scope.
    let other = fixture.root.join("workspaces/other");
    fs::create_dir_all(&other).unwrap();
    let (_, scoped) = fixture.workspace_key();
    let (status, _) = fixture
        .call(
            "POST",
            "/api/v1/conversations",
            Some(&scoped),
            Some(json!({ "agent": "antigravity", "workspace": other })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An unscoped key may only use workspaces a paired device trusted.
    let (_, unscoped) = fixture.key(ApiKeyScopes::default());
    let (status, body) = fixture
        .call(
            "POST",
            "/api/v1/conversations",
            Some(&unscoped),
            Some(create("antigravity")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "WORKSPACE_TRUST_REQUIRED");
    fixture
        .state
        .workspace_trust
        .set_owned(DEVICE_OWNER, &fixture.workspace, true)
        .await
        .unwrap();
    let (status, body) = fixture
        .call(
            "POST",
            "/api/v1/conversations",
            Some(&unscoped),
            Some(create("antigravity")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["agent"], "antigravity");
    assert!(body.get("ownerId").is_none());
}

#[tokio::test]
async fn runs_return_plaintext_while_history_stays_encrypted_and_isolated() {
    let fixture = fixture().await;
    let (record, key) = fixture.workspace_key();
    let (status, run) = fixture
        .call(
            "POST",
            "/api/v1/runs",
            Some(&key),
            Some(json!({
                "agent": "antigravity",
                "workspace": fixture.workspace,
                "text": "hello",
                "title": "Secret title",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["output"], "Hi there");
    let conversation_id = run["conversationId"].as_str().unwrap().to_owned();

    let (status, page) = fixture
        .call(
            "GET",
            &format!("/api/v1/conversations/{conversation_id}/events"),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let events = page["events"].as_array().unwrap();
    assert!(!events.is_empty());
    assert!(!page.to_string().contains("$enc"));
    assert!(events.iter().any(|event| event["type"] == "turn.completed"));
    // Streamed fragments are journalled merged.
    assert!(events
        .iter()
        .any(|event| event["payload"].pointer("/delta/text") == Some(&json!("Hi there"))));
    let (_, manifest) = fixture
        .call(
            "GET",
            &format!("/api/v1/conversations/{conversation_id}"),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(manifest["title"], "Secret title");

    // On disk the content is ciphertext only, and every key the
    // conversation used is wrapped for this API key's recipient.
    let directory = fixture
        .state
        .config
        .data_dir
        .join("conversations")
        .join(&conversation_id);
    let bytes = all_bytes(&directory);
    assert!(!contains(&bytes, "Hi there"));
    assert!(!contains(&bytes, "Secret title"));
    let rid = crate::history_crypto::RecipientPublicKey::from_base64url(&record.history_public_key)
        .unwrap()
        .rid();
    let keyring = fixture
        .state
        .history_keys
        .keyrings()
        .keys(&conversation_id)
        .await
        .unwrap();
    assert!(!keyring.is_empty());
    assert!(keyring
        .iter()
        .all(|entry| entry.wraps.iter().any(|wrap| wrap.rid == rid)));

    // Another key sees nothing of it, and cannot decrypt it either.
    let (_, other) = fixture.workspace_key();
    let (status, _) = fixture
        .call(
            "GET",
            &format!("/api/v1/conversations/{conversation_id}/events"),
            Some(&other),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = fixture
        .call("GET", "/api/v1/conversations", Some(&other), None)
        .await;
    assert_eq!(list["conversations"], json!([]));
    assert!(fixture
        .state
        .conversations
        .list_owned(DEVICE_OWNER)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn turns_stream_their_events_as_sse_and_resume_by_last_event_id() {
    let fixture = fixture().await;
    let (_, key) = fixture.workspace_key();
    let (status, manifest) = fixture
        .call(
            "POST",
            "/api/v1/conversations",
            Some(&key),
            Some(json!({ "agent": "antigravity", "workspace": fixture.workspace })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = manifest["id"].as_str().unwrap().to_owned();
    let (status, body) = fixture
        .raw(
            "POST",
            &format!("/api/v1/conversations/{id}/turns"),
            Some(&key),
            Some(json!({ "text": "hello" })),
            Some("text/event-stream"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let events = sse_events(&body);
    let (_, last_type, last) = events.last().unwrap();
    assert_eq!(last_type, "turn.completed");
    let turn_id = last["payload"]["turnId"].as_str().unwrap().to_owned();
    let ids = events.iter().filter_map(|(id, ..)| *id).collect::<Vec<_>>();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    assert!(events
        .iter()
        .any(|(_, kind, data)| kind == "turn.started" && data["payload"]["turnId"] == turn_id));
    assert!(!String::from_utf8_lossy(&body).contains("$enc"));

    // A second turn while none runs, then a non-streamed one: 202.
    let (status, accepted) = fixture
        .call(
            "POST",
            &format!("/api/v1/conversations/{id}/turns"),
            Some(&key),
            Some(json!({ "text": "hello again" })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert!(accepted["turnId"].is_string());

    // Prompt fields the key's policy governs are refused.
    let (status, _) = fixture
        .call(
            "POST",
            &format!("/api/v1/conversations/{id}/turns"),
            Some(&key),
            Some(json!({ "text": "x", "permissionMode": "bypassPermissions" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The full event page, then a resume from the middle.
    let (_, page) = fixture
        .call(
            "GET",
            &format!("/api/v1/conversations/{id}/events?limit=2"),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["hasMore"], true);
    let after = page["nextSequence"].as_u64().unwrap();
    let (_, rest) = fixture
        .call(
            "GET",
            &format!("/api/v1/conversations/{id}/events?after={after}"),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(rest["events"][0]["sequence"].as_u64().unwrap(), after + 1);
}

#[tokio::test]
async fn standing_policies_answer_only_their_keys_conversations() {
    let fixture = fixture().await;
    let policy = ApiKeyPermissionPolicy {
        state_conversations: fixture.state.conversation_store().clone(),
        keys: fixture.state.api_keys.clone(),
    };
    let (record, _) = fixture.workspace_key();
    let manifest = fixture
        .state
        .conversations
        .create_owned(
            &record.owner_id(),
            ProviderKind::Antigravity,
            fixture.workspace.clone(),
            None,
            None,
        )
        .await
        .unwrap();
    let options = json!([
        { "optionId": "yes", "kind": "allow_once", "name": "Allow" },
        { "optionId": "no", "kind": "reject_once", "name": "Reject" },
    ]);
    let decide = |device_bound| policy.decide(&manifest.id, &options, device_bound);
    assert!(decide(false).await.is_none(), "ask leaves it to the caller");

    for (approval, device_bound, expected) in [
        (ApprovalPolicy::AutoApprove, false, "yes"),
        (ApprovalPolicy::AutoApprove, true, "no"),
        (ApprovalPolicy::Reject, false, "no"),
    ] {
        fixture
            .state
            .api_keys
            .update(
                &record.id,
                crate::api_keys::ApiKeyUpdate {
                    approval: Some(approval),
                    ..Default::default()
                },
            )
            .unwrap();
        let (decision, principal) = decide(device_bound).await.unwrap();
        assert_eq!(decision.option_id.as_deref(), Some(expected));
        assert_eq!(principal, format!("apikey-policy:{}", record.id));
    }

    // A device conversation is never answered by a key's policy.
    let device = fixture
        .state
        .conversations
        .create_owned(
            DEVICE_OWNER,
            ProviderKind::Antigravity,
            fixture.workspace.clone(),
            None,
            None,
        )
        .await;
    // Without a device history recipient the daemon refuses device
    // conversations; either way no policy applies.
    if let Ok(device) = device {
        assert!(policy.decide(&device.id, &options, false).await.is_none());
    }
}
