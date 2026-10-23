//! A container deployment against the real binary: `/v2/version` reports it
//! without a credential, `update` changes nothing, and the headless admin
//! commands (`pair`, `devices`) answer with their documented exit codes.
#![cfg(unix)]

use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

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

fn container_command(data_dir: &PathBuf) -> Command {
    let mut command = Command::new(BIN);
    command
        .env("TODEX_AGENTD_DATA_DIR", data_dir)
        .env("TODEX_AGENTD_DEPLOYMENT", "container")
        // Blank values (compose passes unset variables) count as unset.
        .env("TODEX_AGENTD_PATH_MAPPINGS", "")
        .env("TODEX_AGENTD_IMAGE_CLI_DIR", "")
        .env_remove("TODEX_AGENTD_ADVERTISE_URL");
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run todex-agentd")
}

#[tokio::test]
async fn container_deployment_reports_itself_and_admin_commands_use_exit_codes() {
    let root = std::env::temp_dir().join(format!("todex-e2e-container-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("workspace")).unwrap();
    let data_dir = root.join("data");

    let port = free_port();
    let child = container_command(&data_dir)
        .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
        .arg("--workspace-root")
        .arg(root.join("workspace"))
        .env("TODEX_AGENTD_ADVERTISE_URL", "http://127.0.0.1:7345")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn todex-agentd");
    let _daemon = Daemon {
        child,
        root: root.clone(),
    };
    let client = reqwest::Client::new();
    let started = Instant::now();
    let version: Value = loop {
        match client
            .get(format!("http://127.0.0.1:{port}/v2/version"))
            .send()
            .await
        {
            Ok(response) => break response.json().await.unwrap(),
            Err(_) => {
                assert!(
                    started.elapsed() < Duration::from_secs(30),
                    "daemon did not start"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    assert_eq!(version["deployment"], "container", "{version}");
    assert!(version.get("data_dir").is_none());

    // The daemon created its pairing control directory; nothing is waiting.
    let list = run(container_command(&data_dir).args(["pair", "list", "--format", "json"]));
    assert!(list.status.success(), "{list:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&list.stdout).unwrap(),
        serde_json::json!([])
    );
    let approve = run(container_command(&data_dir).args(["pair", "approve", "ABCDE-FGHJK"]));
    assert_eq!(approve.status.code(), Some(2), "{approve:?}");
    let revoke = run(container_command(&data_dir).args(["devices", "revoke", "dev_missing"]));
    assert_eq!(revoke.status.code(), Some(2), "{revoke:?}");
    let devices = run(container_command(&data_dir).args(["devices", "list", "--format", "json"]));
    assert!(devices.status.success(), "{devices:?}");

    // The image is the update unit: `update` changes nothing and succeeds.
    let update = run(container_command(&data_dir).arg("update"));
    assert!(update.status.success(), "{update:?}");
    assert!(String::from_utf8_lossy(&update.stdout).contains("managed by the container image"));

    // An invalid setting stops the command like any other config error.
    let invalid = run(container_command(&data_dir).args(["pair", "list"]).env(
        "TODEX_AGENTD_ADVERTISE_URL",
        "http://user:pw@127.0.0.1:7345",
    ));
    assert_eq!(invalid.status.code(), Some(1), "{invalid:?}");
}
