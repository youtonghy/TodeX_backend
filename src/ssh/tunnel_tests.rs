//! Port forwards and the IP pool against fake `ssh` scripts.

use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    forward::{ForwardInput, ForwardKind, ForwardStatus},
    ip_pool::{AssignmentInput, AssignmentState, AssignmentUpdate, PoolConfigInput, PortMode},
    tests::{fixture, Fixture},
    *,
};

/// Replaces the fixture's fake ssh with `body` (after the shared prelude
/// that logs argv and answers `-G`).
fn fake_ssh(fixture: &Fixture, body: &str) {
    let path = fixture.root.join("fake-ssh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
case " $* " in
  *" -G "*) printf 'hostname x.example\nuser ops\nport 22\n'; exit 0;;
esac
{body}
"#,
            log = fixture.root.join("ssh.log").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn log(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.root.join("ssh.log")).unwrap_or_default()
}

fn managed(alias: &str) -> ManagedHost {
    ManagedHost {
        alias: alias.into(),
        host_name: "10.0.0.5".into(),
        ..ManagedHost::default()
    }
}

fn local_forward(alias: &str, port: u16) -> ForwardInput {
    ForwardInput {
        alias: alias.into(),
        kind: ForwardKind::Local,
        bind_address: None,
        bind_port: port.into(),
        target_host: Some("localhost".into()),
        target_port: Some(80),
        label: Some("web".into()),
    }
}

fn free_port(ip: std::net::IpAddr) -> u16 {
    std::net::TcpListener::bind((ip, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Reports a listener like OpenSSH at LogLevel=DEBUG1, then stays up.
const READY_SSH: &str = r#"echo "debug1: Authenticated to x ([10.0.0.5]:22) using \"publickey\"." >&2
echo "debug1: Local forwarding listening on 127.0.0.1 port 1." >&2
echo "debug1: remote forward success for: listen 8080, connect localhost:80" >&2
exec sleep 30
"#;

#[tokio::test]
async fn forwards_run_a_dedicated_ssh_and_stop_it() {
    let fixture = fixture("Host good\n").await;
    fake_ssh(&fixture, READY_SSH);
    let port = free_port(std::net::Ipv4Addr::LOCALHOST.into());
    let forward = fixture
        .service
        .create_forward(local_forward("good", port))
        .await
        .unwrap();
    assert_eq!(forward.run.status, ForwardStatus::Stopped);
    let json = serde_json::to_value(&forward).unwrap();
    assert_eq!(json["bindAddress"], "127.0.0.1");
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["label"], "web");

    let started = fixture
        .service
        .start_forward(&forward.spec.id, None)
        .await
        .unwrap();
    assert_eq!(started.run.status, ForwardStatus::Running, "{started:?}");
    assert!(started.run.started_at.is_some());
    let line = log(&fixture)
        .lines()
        .find(|line| line.contains(" -L "))
        .unwrap()
        .to_owned();
    let rest = line
        .split_once(&format!("{} ", fixture.service.wrapper_path().display()))
        .unwrap()
        .1;
    assert!(
        rest.starts_with(
            "-o ControlMaster=no -o ControlPath=none -o BatchMode=yes -o \
             StrictHostKeyChecking=yes -o ConnectTimeout=15 -N -o ExitOnForwardFailure=yes \
             -o LogLevel=DEBUG1 -L "
        ),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("-L 127.0.0.1:{port}:localhost:80 -- good")),
        "{line}"
    );
    // Running forwards cannot be edited.
    assert!(matches!(
        fixture
            .service
            .update_forward(&forward.spec.id, local_forward("good", port))
            .await,
        Err(AppError::Conflict(_))
    ));
    let stopped = fixture
        .service
        .stop_forward(&forward.spec.id)
        .await
        .unwrap();
    assert_eq!(stopped.run.status, ForwardStatus::Stopped);
    fixture
        .service
        .update_forward(&forward.spec.id, local_forward("good", port))
        .await
        .unwrap();

    // Remote forwards wait for the server's confirmation.
    let remote = fixture
        .service
        .create_forward(ForwardInput {
            kind: ForwardKind::Remote,
            ..local_forward("good", 8080)
        })
        .await
        .unwrap();
    let started = fixture
        .service
        .start_forward(&remote.spec.id, None)
        .await
        .unwrap();
    assert_eq!(started.run.status, ForwardStatus::Running, "{started:?}");
    assert!(log(&fixture).contains("-R 8080:localhost:80 -- good"));
    fixture.service.shutdown().await;
    assert_eq!(
        fixture.service.list_forwards().await[1].run.status,
        ForwardStatus::Stopped
    );
}

#[tokio::test]
async fn forward_failures_are_reported_in_the_state() {
    let fixture = fixture("Host good\n").await;
    fake_ssh(
        &fixture,
        r#"echo "ops@x: Permission denied (publickey,password)." >&2; exit 255"#,
    );
    let port = free_port(std::net::Ipv4Addr::LOCALHOST.into());
    let forward = fixture
        .service
        .create_forward(local_forward("good", port))
        .await
        .unwrap();
    let failed = fixture
        .service
        .start_forward(&forward.spec.id, None)
        .await
        .unwrap();
    assert_eq!(failed.run.status, ForwardStatus::Failed);
    assert_eq!(
        failed.run.failure,
        Some(SshFailureKind::AuthenticationFailed)
    );
    assert!(failed
        .run
        .error
        .as_deref()
        .unwrap()
        .contains("Permission denied"));

    // A taken local port fails before ssh even runs.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = fixture
        .service
        .create_forward(local_forward("good", taken.local_addr().unwrap().port()))
        .await
        .unwrap();
    let runs_before = log(&fixture).lines().count();
    let failed = fixture
        .service
        .start_forward(&busy.spec.id, None)
        .await
        .unwrap();
    assert_eq!(failed.run.status, ForwardStatus::Failed);
    assert!(
        failed
            .run
            .error
            .as_deref()
            .unwrap()
            .contains("cannot listen"),
        "{failed:?}"
    );
    assert_eq!(log(&fixture).lines().count(), runs_before);

    // A forward that dies later turns failed with its reason.
    fake_ssh(
        &fixture,
        r#"echo "debug1: Local forwarding listening on 127.0.0.1 port 1." >&2
sleep 0.3; echo "Connection to x closed by remote host." >&2; exit 255"#,
    );
    let running = fixture
        .service
        .start_forward(&forward.spec.id, None)
        .await
        .unwrap();
    assert_eq!(running.run.status, ForwardStatus::Running);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let view = &fixture.service.list_forwards().await[0];
    assert_eq!(view.run.status, ForwardStatus::Failed);
    assert!(
        view.run
            .error
            .as_deref()
            .unwrap()
            .contains("closed by remote host"),
        "{view:?}"
    );
}

#[tokio::test]
async fn password_forwards_use_askpass_on_their_own_connection() {
    let fixture = fixture("Host good\n").await;
    fake_ssh(&fixture, READY_SSH);
    let port = free_port(std::net::Ipv4Addr::LOCALHOST.into());
    let forward = fixture
        .service
        .create_forward(local_forward("good", port))
        .await
        .unwrap();
    let started = fixture
        .service
        .start_forward(&forward.spec.id, Some("hunter2"))
        .await
        .unwrap();
    assert_eq!(started.run.status, ForwardStatus::Running);
    let log = log(&fixture);
    let line = log.lines().find(|line| line.contains(" -L ")).unwrap();
    assert!(
        line.contains(
            "-o ControlMaster=no -o ControlPath=none -o BatchMode=no -o NumberOfPasswordPrompts=1"
        ),
        "{line}"
    );
    assert!(!log.contains("hunter2"));
    // The password file is gone once ssh is ready.
    assert!(tests::askpass_files(&fixture).is_empty());
    fixture.service.shutdown().await;
}

#[tokio::test]
async fn deleting_a_host_removes_its_forwards_and_address() {
    let fixture = fixture("").await;
    fake_ssh(&fixture, READY_SSH);
    fixture.service.create_host(managed("db")).await.unwrap();
    fixture.service.create_host(managed("web")).await.unwrap();
    let port = free_port(std::net::Ipv4Addr::LOCALHOST.into());
    let forward = fixture
        .service
        .create_forward(local_forward("db", port))
        .await
        .unwrap();
    fixture
        .service
        .create_forward(local_forward("web", 1))
        .await
        .unwrap();
    fixture
        .service
        .start_forward(&forward.spec.id, None)
        .await
        .unwrap();
    for alias in ["db", "web"] {
        fixture
            .service
            .create_ip_assignment(AssignmentInput {
                ip: None,
                alias: alias.into(),
                port_mode: PortMode::Auto,
                ports: None,
            })
            .await
            .unwrap();
    }
    // Renames carry over.
    fixture
        .service
        .update_host("web", managed("web2"))
        .await
        .unwrap();
    fixture.service.delete_host("db").await.unwrap();
    let forwards = fixture.service.list_forwards().await;
    assert_eq!(forwards.len(), 1);
    assert_eq!(forwards[0].spec.alias, "web2");
    let pool = fixture.service.ip_pool().await;
    assert_eq!(pool.assignments.len(), 1);
    assert_eq!(pool.assignments[0].alias, "web2");
    assert!(!fixture.service.inner.forwards.is_active(&forward.spec.id));
    // Unknown hosts cannot get forwards or addresses.
    assert!(matches!(
        fixture.service.create_forward(local_forward("db", 2)).await,
        Err(AppError::NotFound(_))
    ));
}

#[tokio::test]
async fn forwards_and_pool_survive_a_restart_and_old_daemons_read_the_store() {
    let fixture = fixture("Host good\n").await;
    let forward = fixture
        .service
        .create_forward(local_forward("good", 9999))
        .await
        .unwrap();
    fixture
        .service
        .create_ip_assignment(AssignmentInput {
            ip: Some("127.77.0.9".into()),
            alias: "good".into(),
            port_mode: PortMode::Manual,
            ports: Some(vec!["22".into()]),
        })
        .await
        .unwrap();
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root.join("data/ssh/store.json")).unwrap())
            .unwrap();
    assert_eq!(stored["forwards"][0]["id"], forward.spec.id.as_str());
    assert!(stored["forwards"][0].get("status").is_none());
    assert_eq!(stored["ipPool"]["assignments"][0]["ip"], "127.77.0.9");
    // Older daemons ignore unknown top-level fields.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct OldStore {
        #[serde(default)]
        managed_hosts: Vec<serde_json::Value>,
        #[serde(default)]
        agent_access: BTreeSet<String>,
    }
    serde_json::from_value::<OldStore>(stored).unwrap();

    let reloaded = SshService::with_home(
        &fixture.root.join("data"),
        "ssh".into(),
        Some(fixture.root.join("home")),
    )
    .await
    .unwrap();
    let forwards = reloaded.list_forwards().await;
    assert_eq!(forwards.len(), 1);
    // Never started on their own.
    assert_eq!(forwards[0].run.status, ForwardStatus::Stopped);
    let pool = reloaded.ip_pool().await;
    assert_eq!(pool.assignments[0].ports, ["22"]);
    assert_eq!(pool.assignments[0].state, AssignmentState::Disabled);
}

/// Fake ssh for the pool: the master creates its control "socket" and
/// waits; `-O exit` ends it; `-W` clients answer with their target.
fn pool_ssh(fixture: &Fixture) {
    let pid = fixture.root.join("master.pid");
    fake_ssh(
        fixture,
        &format!(
            r#"ctl=""; prev=""; target=""
for arg in "$@"; do
  case "$arg" in ControlPath=*) ctl="${{arg#ControlPath=}}";; esac
  [ "$prev" = "-W" ] && target="$arg"
  prev="$arg"
done
case " $* " in
  *" -O exit "*) kill "$(cat "{pid}")"; exit 0;;
  *" ControlMaster=yes "*) echo $$ > "{pid}"; : > "$ctl"; exec sleep 60;;
  *" -W "*) [ -e "$ctl" ] || exit 255; echo "via $target"; cat > /dev/null; exit 0;;
esac
exit 1
"#,
            pid = pid.display()
        ),
    );
}

async fn read_line(stream: &mut tokio::net::TcpStream) -> String {
    let mut buffer = vec![0u8; 128];
    let read = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&buffer[..read]).into_owned()
}

async fn enable_pool(service: &SshService) {
    service
        .set_ip_pool(PoolConfigInput {
            enabled: true,
            cidr: "127.77.0.0/24".into(),
            idle_minutes: 15,
        })
        .await
        .unwrap();
}

/// Linux routes all of 127.0.0.0/8 to loopback; macOS needs aliases.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn pool_starts_a_master_on_demand_and_closes_it_when_idle() {
    let fixture = fixture("").await;
    pool_ssh(&fixture);
    fixture
        .service
        .inner
        .pool
        .set_idle_override(Duration::from_millis(400));
    fixture.service.create_host(managed("db")).await.unwrap();
    let ip: std::net::Ipv4Addr = "127.77.0.23".parse().unwrap();
    let port = free_port(ip.into());
    let taken = std::net::TcpListener::bind((ip, free_port(ip.into()))).unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    fixture
        .service
        .create_ip_assignment(AssignmentInput {
            ip: Some(ip.to_string()),
            alias: "db".into(),
            port_mode: PortMode::Manual,
            ports: Some(vec![port.to_string(), taken_port.to_string()]),
        })
        .await
        .unwrap();
    enable_pool(&fixture.service).await;
    let view = &fixture.service.ip_pool().await.assignments[0];
    assert_eq!(view.state, AssignmentState::Idle);
    assert_eq!(view.listening_ports, 1);
    assert_eq!(view.skipped_ports, [taken_port]);
    assert_eq!(view.skipped_port_count, 1);

    let mut stream = tokio::net::TcpStream::connect((ip, port)).await.unwrap();
    assert_eq!(
        read_line(&mut stream).await,
        format!("via 127.0.0.1:{port}\n")
    );
    let view = fixture.service.ip_pool().await.assignments[0].clone();
    assert_eq!(view.state, AssignmentState::Connected);
    assert_eq!(view.active_connections, 1);
    assert!(view.connected_at.is_some());
    // A second connection reuses the master.
    let mut second = tokio::net::TcpStream::connect((ip, port)).await.unwrap();
    assert_eq!(
        read_line(&mut second).await,
        format!("via 127.0.0.1:{port}\n")
    );
    let log_text = log(&fixture);
    assert_eq!(
        log_text.matches("ControlMaster=yes").count(),
        1,
        "{log_text}"
    );
    let master = log_text
        .lines()
        .find(|line| line.contains("ControlMaster=yes"))
        .unwrap();
    assert!(master.contains("-o BatchMode=yes"), "{master}");
    assert!(
        master.ends_with("-N -o ClearAllForwardings=yes -- db"),
        "{master}"
    );
    let client = log_text.lines().find(|line| line.contains(" -W ")).unwrap();
    assert!(
        client.contains("-o ControlMaster=no -o ControlPath=")
            && client.contains("-o ProxyCommand=false")
            && client.ends_with(&format!("-W 127.0.0.1:{port} -- db")),
        "{client}"
    );

    // Open connections keep the master past the idle time.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        fixture.service.ip_pool().await.assignments[0].state,
        AssignmentState::Connected
    );
    stream.shutdown().await.unwrap();
    drop(stream);
    drop(second);
    let service = fixture.service.clone();
    let mut idle = false;
    for _ in 0..60 {
        let view = service.ip_pool().await.assignments[0].clone();
        if view.state == AssignmentState::Idle && view.active_connections == 0 {
            idle = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(idle, "{:?}", service.ip_pool().await.assignments[0]);
    assert!(log(&fixture).contains("-O exit -- db"));
    assert!(service.ip_pool().await.assignments[0]
        .last_used_at
        .is_some());

    // Ports change incrementally; the freed port gets a listener.
    drop(taken);
    let extra = free_port(ip.into());
    fixture
        .service
        .update_ip_assignment(
            ip,
            AssignmentUpdate {
                port_mode: PortMode::Manual,
                ports: Some(vec![
                    port.to_string(),
                    taken_port.to_string(),
                    extra.to_string(),
                ]),
            },
        )
        .await
        .unwrap();
    let view = fixture.service.ip_pool().await.assignments[0].clone();
    assert_eq!(view.listening_ports, 3);
    assert!(view.skipped_ports.is_empty());
    let mut stream = tokio::net::TcpStream::connect((ip, extra)).await.unwrap();
    assert_eq!(
        read_line(&mut stream).await,
        format!("via 127.0.0.1:{extra}\n")
    );
    // Explicit disconnect closes the master; listeners stay.
    let view = fixture.service.disconnect_ip_assignment(ip).await.unwrap();
    assert_eq!(view.state, AssignmentState::Idle);
    assert_eq!(view.listening_ports, 3);

    // Disabling stops the listeners.
    fixture
        .service
        .set_ip_pool(PoolConfigInput {
            enabled: false,
            cidr: "127.77.0.0/24".into(),
            idle_minutes: 15,
        })
        .await
        .unwrap();
    let view = fixture.service.ip_pool().await.assignments[0].clone();
    assert_eq!(view.state, AssignmentState::Disabled);
    assert_eq!(view.listening_ports, 0);
    assert!(tokio::net::TcpStream::connect((ip, port)).await.is_err());
    fixture.service.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn password_hosts_need_an_explicit_login() {
    let fixture = fixture("").await;
    pool_ssh(&fixture);
    fixture
        .service
        .create_host(ManagedHost {
            auth_method: SshAuthMethod::Password,
            ..managed("pw")
        })
        .await
        .unwrap();
    let ip: std::net::Ipv4Addr = "127.77.0.24".parse().unwrap();
    let port = free_port(ip.into());
    fixture
        .service
        .create_ip_assignment(AssignmentInput {
            ip: Some(ip.to_string()),
            alias: "pw".into(),
            port_mode: PortMode::Manual,
            ports: Some(vec![port.to_string()]),
        })
        .await
        .unwrap();
    enable_pool(&fixture.service).await;
    let mut stream = tokio::net::TcpStream::connect((ip, port)).await.unwrap();
    let mut buffer = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .unwrap();
    assert!(matches!(read, Ok(0) | Err(_)), "closed without a login");
    let view = fixture.service.ip_pool().await.assignments[0].clone();
    assert_eq!(view.state, AssignmentState::NeedsLogin);
    assert!(!log(&fixture).contains("ControlMaster=yes"));

    let view = fixture
        .service
        .connect_ip_assignment(ip, Some("hunter2"))
        .await
        .unwrap();
    assert_eq!(view.state, AssignmentState::Connected, "{view:?}");
    let log_text = log(&fixture);
    let master = log_text
        .lines()
        .find(|line| line.contains("ControlMaster=yes"))
        .unwrap();
    assert!(
        master.contains("-o BatchMode=no -o NumberOfPasswordPrompts=1"),
        "{master}"
    );
    assert!(!log_text.contains("hunter2"));
    let mut stream = tokio::net::TcpStream::connect((ip, port)).await.unwrap();
    assert_eq!(
        read_line(&mut stream).await,
        format!("via 127.0.0.1:{port}\n")
    );
    fixture.service.shutdown().await;
}

#[tokio::test]
async fn pool_config_is_validated() {
    let fixture = fixture("Host good\n").await;
    let pool = fixture.service.ip_pool().await;
    assert!(!pool.enabled);
    assert_eq!(pool.cidr, "127.77.0.0/24");
    assert_eq!(pool.idle_minutes, 15);
    assert_eq!(pool.manual_port_limit, 4096);
    assert!(pool.auto_ports.contains(&"8000-8100".to_owned()));
    let json = serde_json::to_value(&pool).unwrap();
    assert_eq!(json["supported"], true);
    assert!(json.get("unsupportedReason").is_none());
    fixture
        .service
        .create_ip_assignment(AssignmentInput {
            ip: Some("127.77.0.200".into()),
            alias: "good".into(),
            port_mode: PortMode::Auto,
            ports: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .service
            .set_ip_pool(PoolConfigInput {
                enabled: false,
                cidr: "127.77.0.0/25".into(),
                idle_minutes: 15,
            })
            .await,
        Err(AppError::Conflict(_))
    ));
    let missing: std::net::Ipv4Addr = "127.77.0.7".parse().unwrap();
    assert!(matches!(
        fixture.service.disconnect_ip_assignment(missing).await,
        Err(AppError::NotFound(_))
    ));
    // Connecting needs an enabled pool.
    assert!(matches!(
        fixture
            .service
            .connect_ip_assignment("127.77.0.200".parse().unwrap(), None)
            .await,
        Err(AppError::Conflict(_))
    ));
}

/// Manual end-to-end check against a real sshd on 127.0.0.1 (Linux):
/// `TODEX_SSH_E2E_DIR=<dir> TODEX_SSH_E2E_PORT=<port> TODEX_SSH_E2E_HTTP=<port>
/// cargo test real_ssh_forwards_and_pool -- --nocapture`. `<dir>` holds
/// `clientkey` (authorized on the server) and `known_hosts` with the server's
/// key; an HTTP server on 127.0.0.1:<http> serves `hello.txt`, and another
/// one must listen on 127.0.0.1:8050 (an auto-mode port).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_ssh_forwards_and_pool() {
    let Some(dir) = std::env::var_os("TODEX_SSH_E2E_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    let port: u16 = std::env::var("TODEX_SSH_E2E_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let http: u16 = std::env::var("TODEX_SSH_E2E_HTTP")
        .unwrap()
        .parse()
        .unwrap();
    let root = std::env::temp_dir().join(format!("todex-ssh-e2e-{}", Uuid::new_v4().simple()));
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let service = SshService::with_home(&root.join("data"), "ssh".into(), Some(home))
        .await
        .unwrap();
    let option = |key: &str, value: String| config_file::SshOption {
        key: key.into(),
        value,
    };
    let host = |alias: &str, known_hosts: &Path| ManagedHost {
        alias: alias.into(),
        host_name: "127.0.0.1".into(),
        port: Some(port),
        user: std::env::var("USER").ok(),
        identity_file: Some(dir.join("clientkey").display().to_string()),
        options: vec![
            option("UserKnownHostsFile", known_hosts.display().to_string()),
            option("IdentitiesOnly", "yes".into()),
        ],
        ..ManagedHost::default()
    };
    service
        .create_host(host("e2e", &dir.join("known_hosts")))
        .await
        .unwrap();
    std::fs::write(root.join("empty_known_hosts"), "").unwrap();
    service
        .create_host(host("e2e-unknown", &root.join("empty_known_hosts")))
        .await
        .unwrap();
    let get = |address: String| async move {
        let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
        stream
            .write_all(b"GET /hello.txt HTTP/1.0\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut body = String::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut body))
            .await
            .unwrap()
            .unwrap();
        body
    };
    let free = || free_port(std::net::Ipv4Addr::LOCALHOST.into());
    let start = |input: ForwardInput| {
        let service = service.clone();
        async move {
            let forward = service.create_forward(input).await.unwrap();
            let started = service.start_forward(&forward.spec.id, None).await.unwrap();
            println!("{}", serde_json::to_string(&started).unwrap());
            started
        }
    };

    // -L
    let local_port = free();
    let local = start(ForwardInput {
        target_host: Some("127.0.0.1".into()),
        target_port: Some(http.into()),
        ..local_forward("e2e", local_port)
    })
    .await;
    assert_eq!(local.run.status, ForwardStatus::Running);
    assert!(get(format!("127.0.0.1:{local_port}"))
        .await
        .contains("hello-e2e"));

    // -D (SOCKS through curl)
    let socks_port = free();
    let dynamic = start(ForwardInput {
        kind: ForwardKind::Dynamic,
        target_host: None,
        target_port: None,
        ..local_forward("e2e", socks_port)
    })
    .await;
    assert_eq!(dynamic.run.status, ForwardStatus::Running);
    let curl = std::process::Command::new("curl")
        .args(["-s", "--max-time", "10", "--socks5-hostname"])
        .arg(format!("127.0.0.1:{socks_port}"))
        .arg(format!("http://127.0.0.1:{http}/hello.txt"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&curl.stdout).contains("hello-e2e"));

    // -R (the "server" is this machine, so its listener is reachable here)
    let remote_port = free();
    let remote = start(ForwardInput {
        kind: ForwardKind::Remote,
        target_host: Some("127.0.0.1".into()),
        target_port: Some(http.into()),
        ..local_forward("e2e", remote_port)
    })
    .await;
    assert_eq!(remote.run.status, ForwardStatus::Running);
    assert!(get(format!("127.0.0.1:{remote_port}"))
        .await
        .contains("hello-e2e"));

    // Local port in use, remote port in use, unknown host key.
    let busy = start(local_forward("e2e", http)).await;
    assert_eq!(busy.run.status, ForwardStatus::Failed);
    // Taken on IPv4 and IPv6 (sshd succeeds if either loopback binds).
    let taken = std::net::TcpListener::bind("[::]:0").unwrap();
    let remote_busy = start(ForwardInput {
        kind: ForwardKind::Remote,
        ..local_forward("e2e", taken.local_addr().unwrap().port())
    })
    .await;
    assert_eq!(remote_busy.run.status, ForwardStatus::Failed);
    let unknown = start(local_forward("e2e-unknown", free())).await;
    assert_eq!(unknown.run.failure, Some(SshFailureKind::HostKeyUnverified));

    // Stop kills the ssh process: the listener goes away.
    service.stop_forward(&local.spec.id).await.unwrap();
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", local_port))
        .await
        .is_err());

    // IP pool: auto mode reaches 127.0.0.1:8050 on the server.
    service.inner.pool.set_idle_override(Duration::from_secs(2));
    let ip: std::net::Ipv4Addr = "127.77.0.2".parse().unwrap();
    service
        .create_ip_assignment(AssignmentInput {
            ip: Some(ip.to_string()),
            alias: "e2e".into(),
            port_mode: PortMode::Auto,
            ports: None,
        })
        .await
        .unwrap();
    enable_pool(&service).await;
    let view = service.ip_pool().await.assignments[0].clone();
    println!("{}", serde_json::to_string(&view).unwrap());
    assert!(view.listening_ports > 200);
    assert!(get(format!("{ip}:8050")).await.contains("hello-8050"));
    assert_eq!(
        service.ip_pool().await.assignments[0].state,
        AssignmentState::Connected
    );
    // Manual mode with the test server's port.
    service
        .update_ip_assignment(
            ip,
            AssignmentUpdate {
                port_mode: PortMode::Manual,
                ports: Some(vec![http.to_string()]),
            },
        )
        .await
        .unwrap();
    assert_eq!(service.ip_pool().await.assignments[0].listening_ports, 1);
    assert!(get(format!("{ip}:{http}")).await.contains("hello-e2e"));
    assert!(tokio::net::TcpStream::connect((ip, 8050)).await.is_err());
    // Idle disconnect.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let view = service.ip_pool().await.assignments[0].clone();
    println!("{}", serde_json::to_string(&view).unwrap());
    assert_eq!(view.state, AssignmentState::Idle);
    assert!(!root.join("data/ssh/pool").join(ip.to_string()).exists());
    // The next connection signs in again.
    assert!(get(format!("{ip}:{http}")).await.contains("hello-e2e"));
    service.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}
