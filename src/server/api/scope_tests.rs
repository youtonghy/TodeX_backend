//! The global agent calling scope (`agent-calling.json`) on `/api/v1`: it
//! intersects every key's own scope.

use std::{fs, path::PathBuf};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::agent_calls::AgentCallingUpdate;
use crate::api_keys::{ApiKeyScopes, NewApiKey};
use crate::config::Config;

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

/// Like `tests::fixture`: Antigravity is the stream-json fixture, which
/// lists `gemini-3.8-flash` (efforts `high`, `low`) and logs its argv.
async fn fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;

    let root =
        std::env::temp_dir().join(format!("todex-api-scope-{}", uuid::Uuid::new_v4().simple()));
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
    fn key(&self, agents: Option<Vec<ProviderKind>>) -> String {
        let (_, key) = self
            .state
            .api_keys
            .create(NewApiKey {
                name: "scope".to_owned(),
                scopes: ApiKeyScopes {
                    agents,
                    workspaces: None,
                },
                ..NewApiKey::default()
            })
            .unwrap();
        key.to_string()
    }

    async fn scope(&self, scope: Value) {
        let update: AgentCallingUpdate = serde_json::from_value(json!({ "scope": scope })).unwrap();
        self.state.agent_calling.update(update).await.unwrap();
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {key}"));
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn run(&self, extra: Value) -> Value {
        let mut body = json!({
            "agent": "antigravity",
            "workspace": self.workspace,
            "text": "hello",
        });
        if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
            body.extend(extra);
        }
        body
    }

    /// The `--model` values the fixture was launched with.
    fn launched_models(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("argv.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Vec<String>>(line).ok())
            .filter_map(|argv| {
                let index = argv.iter().position(|arg| arg == "--model")?;
                argv.get(index + 1).cloned()
            })
            .collect()
    }
}

fn agent_ids(body: &Value) -> Vec<String> {
    body["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_global_agent_scope_intersects_key_scopes() {
    let fixture = fixture().await;
    let unscoped = fixture.key(None);
    let antigravity_only = fixture.key(Some(vec![ProviderKind::Antigravity]));
    let (_, all) = fixture.call("GET", "/api/v1/agents", &unscoped, None).await;
    assert!(agent_ids(&all).contains(&"antigravity".to_owned()));

    fixture.scope(json!({ "agents": ["codex"] })).await;
    let (status, body) = fixture.call("GET", "/api/v1/agents", &unscoped, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(agent_ids(&body), ["codex"]);
    let (_, body) = fixture
        .call("GET", "/api/v1/agents", &antigravity_only, None)
        .await;
    assert_eq!(agent_ids(&body), Vec::<String>::new());

    let create = json!({ "agent": "antigravity", "workspace": fixture.workspace });
    for key in [&unscoped, &antigravity_only] {
        let (status, body) = fixture
            .call("POST", "/api/v1/conversations", key, Some(create.clone()))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "UNAUTHORIZED");
    }
    let (status, body) = fixture
        .call(
            "POST",
            "/api/v1/runs",
            &unscoped,
            Some(fixture.run(json!({}))),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = fixture
        .call(
            "GET",
            &format!(
                "/api/v1/agents/antigravity/models?workspace={}",
                fixture.workspace.display()
            ),
            &unscoped,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Turns in an existing conversation of an agent taken out of scope.
    fixture.scope(json!({ "agents": null })).await;
    let (status, manifest) = fixture
        .call("POST", "/api/v1/conversations", &unscoped, Some(create))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{manifest}");
    fixture.scope(json!({ "agents": [] })).await;
    let (status, body) = fixture
        .call(
            "POST",
            &format!(
                "/api/v1/conversations/{}/turns",
                manifest["id"].as_str().unwrap()
            ),
            &unscoped,
            Some(json!({ "text": "hello" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn allowed_models_gate_turns_resolve_unnamed_models_and_filter_listings() {
    let fixture = fixture().await;
    let key = fixture.key(None);
    fixture
        .scope(json!({ "models": { "antigravity": ["gemini-3.8-flash"] } }))
        .await;

    // A model outside the list is refused before anything is created.
    let (status, body) = fixture
        .call(
            "POST",
            "/api/v1/runs",
            &key,
            Some(fixture.run(json!({ "model": "gpt-oss-120b" }))),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "UNAUTHORIZED");
    // So is an effort the model does not support.
    let (status, body) = fixture
        .call(
            "POST",
            "/api/v1/runs",
            &key,
            Some(fixture.run(json!({ "model": "gemini-3.8-flash", "reasoningEffort": "xhigh" }))),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (_, list) = fixture
        .call("GET", "/api/v1/conversations", &key, None)
        .await;
    assert_eq!(list["conversations"], json!([]));

    // Without a model the turn runs the first allowed one (the catalog
    // names no default).
    let (status, run) = fixture
        .call("POST", "/api/v1/runs", &key, Some(fixture.run(json!({}))))
        .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "completed", "{run}");
    let launched = fixture.launched_models();
    assert!(
        launched
            .iter()
            .any(|model| model.starts_with("gemini-3.8-flash")),
        "{launched:?}"
    );
    let (status, run) = fixture
        .call(
            "POST",
            "/api/v1/runs",
            &key,
            Some(fixture.run(json!({ "model": "gemini-3.8-flash", "reasoningEffort": "low" }))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{run}");

    // Listings carry only allowed models.
    let models_uri = format!(
        "/api/v1/agents/antigravity/models?workspace={}",
        fixture.workspace.display()
    );
    let (status, models) = fixture.call("GET", &models_uri, &key, None).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    assert_eq!(models["models"][0]["id"], "gemini-3.8-flash");
    fixture
        .scope(json!({ "models": { "antigravity": ["another-model"] } }))
        .await;
    let (_, models) = fixture.call("GET", &models_uri, &key, None).await;
    assert_eq!(models["models"], json!([]));
    let (_, agents) = fixture.call("GET", "/api/v1/agents", &key, None).await;
    let antigravity = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == "antigravity")
        .unwrap();
    assert!(antigravity["models"]
        .as_array()
        .unwrap()
        .iter()
        .all(|model| model["id"] == "another-model"));
}
