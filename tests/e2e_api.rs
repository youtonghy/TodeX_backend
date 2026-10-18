//! The external API end to end against the real binary: a key issued through
//! `/v2/api-keys` (what Settings → Agent 调用 uses; device authentication is
//! off here, so loopback requests need no signature) runs an Agent (the
//! Antigravity stream-json fixture) through `/api/v1/runs`, reads plaintext
//! back while the history on disk stays encrypted, and stops working once
//! revoked.
#![cfg(unix)]

use std::{
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_todex-agentd");

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn contains_bytes(directory: &Path, needle: &[u8]) -> bool {
    fs::read_dir(directory).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            contains_bytes(&path, needle)
        } else {
            fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|window| window == needle)
        }
    })
}

#[tokio::test]
async fn api_key_runs_an_agent_and_revocation_locks_it_out() {
    let root = std::env::temp_dir().join(format!("todex-e2e-api-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let workspace = root.join("workspaces/project");
    fs::create_dir_all(&workspace).unwrap();
    let root = fs::canonicalize(&root).unwrap();
    let workspace = fs::canonicalize(&workspace).unwrap();
    let data_dir = root.join("data");
    let agy = root.join("agy");
    fs::write(&agy, include_str!("fixtures/antigravity_stream_fixture.py")).unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o755)).unwrap();

    let (port, api_port) = (free_port(), free_port());
    let child = Command::new(BIN)
        .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["--enable-api", "--api-port", &api_port.to_string()])
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--workspace-root")
        .arg(root.join("workspaces"))
        .env("TODEX_AGENTD_ANTIGRAVITY_BIN", &agy)
        .env("TODEX_AGENTD_ENABLE_AUTH", "false")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn todex-agentd");
    let _daemon = Daemon {
        child,
        root: root.clone(),
    };
    let base = format!("http://127.0.0.1:{api_port}/api/v1");
    let client = reqwest::Client::new();
    let started = Instant::now();
    while client.get(format!("{base}/health")).send().await.is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "API listener did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let device_base = format!("http://127.0.0.1:{port}/v2");
    let created: Value = client
        .post(format!("{device_base}/api-keys"))
        .json(&json!({ "name": "e2e", "scopes": { "workspaces": [workspace] } }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = created["key"].as_str().unwrap_or_default().to_owned();
    // The settings report the listener this daemon runs.
    let calling: Value = client
        .get(format!("{device_base}/agent-calling"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(calling["apiListener"]["listening"], true, "{calling}");
    assert_eq!(calling["apiListener"]["baseUrl"], base);
    assert_eq!(calling["apiListener"]["pendingRestart"], false);
    assert!(key.starts_with("tdx_"), "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    let unauthenticated = client.get(format!("{base}/me")).send().await.unwrap();
    assert_eq!(unauthenticated.status(), 401);

    let run: Value = client
        .post(format!("{base}/runs"))
        .bearer_auth(&key)
        .json(&json!({ "agent": "antigravity", "workspace": workspace, "text": "hello" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["output"], "Hi there");
    assert!(!contains_bytes(
        &data_dir.join("conversations"),
        b"Hi there"
    ));

    let stream = client
        .post(format!("{base}/runs"))
        .bearer_auth(&key)
        .json(&json!({
            "agent": "antigravity", "workspace": workspace, "text": "hello", "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stream.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let body = tokio::time::timeout(Duration::from_secs(30), stream.text())
        .await
        .expect("the turn stream ends")
        .unwrap();
    assert!(body.contains("event: turn.completed"), "{body}");
    assert!(body.contains("Hi there"));
    assert!(!body.contains("$enc"));

    let revoke = client
        .delete(format!("{device_base}/api-keys/{id}"))
        .send()
        .await
        .unwrap();
    assert!(revoke.status().is_success(), "{}", revoke.status());
    let revoked = client
        .get(format!("{base}/me"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 401);
}
