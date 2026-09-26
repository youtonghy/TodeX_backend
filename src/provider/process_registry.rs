//! Provider processes run in their own process group, so a daemon that dies
//! without running destructors (SIGKILL, OOM, power loss) leaves them running.
//! While a server is active, every provider it spawns is recorded in
//! `<data_dir>/provider_processes.json`; the next start kills each recorded
//! group whose leader is still the recorded process, identified by PID *and*
//! start time so a reused PID is never signalled. The file also names the
//! server that owns it; while that server is still alive a second server on
//! the same data directory neither reaps nor takes over its records.
//!
//! Unix only; elsewhere tracking and reaping are no-ops.

#[cfg(unix)]
pub(crate) use unix::{activate, track, TrackedProcess};

#[cfg(not(unix))]
pub(crate) use fallback::{activate, track, TrackedProcess};

#[cfg(not(unix))]
mod fallback {
    use std::path::Path;

    pub(crate) struct TrackedProcess;

    pub(crate) async fn activate(_data_dir: &Path) {}

    pub(crate) async fn track(_pid: u32, _program: &str) -> Option<TrackedProcess> {
        None
    }
}

#[cfg(unix)]
mod unix {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, RwLock};

    use serde::{Deserialize, Serialize};

    const REGISTRY_FILE: &str = "provider_processes.json";
    const SCHEMA_VERSION: u32 = 1;

    /// The registry of the running server; `None` until a server activates it,
    /// so tests and one-shot commands that spawn providers record nothing.
    static ACTIVE: RwLock<Option<Arc<ProcessRegistry>>> = RwLock::new(None);

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct ProcessRecord {
        pub(super) pid: u32,
        pub(super) pgid: u32,
        /// `ps -o lstart=` in UTC, compared verbatim.
        pub(super) start_time: String,
        /// The executable as configured, for diagnostics only.
        pub(super) program: String,
    }

    /// The server process that writes the registry, identified like a
    /// provider by PID and start time.
    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct RegistryOwner {
        pub(super) pid: u32,
        pub(super) start_time: String,
    }

    #[derive(Debug, Default, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RegistryFile {
        schema_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<RegistryOwner>,
        processes: Vec<ProcessRecord>,
    }

    pub(super) struct ProcessRegistry {
        path: PathBuf,
        owner: Option<RegistryOwner>,
        records: Mutex<Vec<ProcessRecord>>,
    }

    /// Removes its record when the provider process has been reaped or killed.
    pub(crate) struct TrackedProcess {
        registry: Arc<ProcessRegistry>,
        pid: u32,
    }

    impl Drop for TrackedProcess {
        fn drop(&mut self) {
            self.registry.remove(self.pid);
        }
    }

    /// Reaps orphans recorded by a previous run of the server on `data_dir`,
    /// then records providers spawned from now on. Call after the listener is
    /// bound, so a second server that cannot start never touches the first
    /// one's providers.
    pub(crate) async fn activate(data_dir: &Path) {
        let path = data_dir.join(REGISTRY_FILE);
        let reaped = match reap_orphans(&path).await {
            Ok(reaped) => reaped,
            Err(owner) => {
                tracing::warn!(
                    owner_pid = owner.pid,
                    "another running server owns this data directory's provider registry; provider crash cleanup is disabled for this server"
                );
                return;
            }
        };
        if reaped > 0 {
            tracing::warn!(
                reaped,
                "killed provider processes orphaned by a previous daemon run"
            );
        }
        let owner = current_owner().await;
        if owner.is_none() {
            tracing::warn!(
                "could not read this server's start time; another server could reap its providers"
            );
        }
        let registry = Arc::new(ProcessRegistry {
            path,
            owner,
            records: Mutex::new(Vec::new()),
        });
        registry.persist(&[]);
        *ACTIVE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(registry);
    }

    /// Records a freshly spawned provider group leader.
    pub(crate) async fn track(pid: u32, program: &str) -> Option<TrackedProcess> {
        let registry = ACTIVE
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        registry.track(pid, program).await
    }

    impl ProcessRegistry {
        #[cfg(test)]
        pub(super) fn new(path: PathBuf, owner: Option<RegistryOwner>) -> Arc<Self> {
            Arc::new(Self {
                path,
                owner,
                records: Mutex::new(Vec::new()),
            })
        }

        pub(super) async fn track(
            self: Arc<Self>,
            pid: u32,
            program: &str,
        ) -> Option<TrackedProcess> {
            let Some(start_time) = process_start_time(pid).await else {
                tracing::warn!(
                    pid,
                    "could not read the provider's start time; it will not be reaped after a crash"
                );
                return None;
            };
            let mut records = self.lock();
            records.retain(|record| record.pid != pid);
            records.push(ProcessRecord {
                pid,
                // Providers are spawned with process_group(0).
                pgid: pid,
                start_time,
                program: program.to_owned(),
            });
            self.persist(&records);
            drop(records);
            Some(TrackedProcess {
                registry: self,
                pid,
            })
        }

        fn remove(&self, pid: u32) {
            let mut records = self.lock();
            let before = records.len();
            records.retain(|record| record.pid != pid);
            if records.len() != before {
                self.persist(&records);
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Vec<ProcessRecord>> {
            self.records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Failing to persist only weakens crash cleanup; the provider itself
        /// is unaffected, so the error is logged rather than propagated.
        fn persist(&self, records: &[ProcessRecord]) {
            let file = RegistryFile {
                schema_version: SCHEMA_VERSION,
                owner: self.owner.clone(),
                processes: records.to_vec(),
            };
            if let Err(error) = write_private_atomic(&self.path, &file) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "failed to persist the provider process registry"
                );
            }
        }
    }

    fn write_private_atomic(path: &Path, file: &RegistryFile) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let temporary = path.with_file_name(format!(
            ".{REGISTRY_FILE}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| {
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            output.write_all(&serde_json::to_vec(file)?)?;
            output.sync_all()?;
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    /// This server as a registry owner, or `None` if its start time can't be read.
    async fn current_owner() -> Option<RegistryOwner> {
        let pid = std::process::id();
        let start_time = process_start_time(pid).await?;
        Some(RegistryOwner { pid, start_time })
    }

    /// Kills every recorded group whose leader still has the recorded start
    /// time, drops the other records, and returns how many groups were killed.
    /// Returns the owner instead when the server that wrote the registry is
    /// still running: its providers are live, not orphaned.
    pub(super) async fn reap_orphans(path: &Path) -> Result<usize, RegistryOwner> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "failed to read the provider process registry");
                return Ok(0);
            }
        };
        let file: RegistryFile = match serde_json::from_slice(&bytes) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "ignoring an unreadable provider process registry");
                return Ok(0);
            }
        };
        if let Some(owner) = file.owner {
            let alive = owner.pid != std::process::id()
                && process_start_time(owner.pid).await.as_deref()
                    == Some(owner.start_time.as_str());
            if alive {
                return Err(owner);
            }
        }
        let mut reaped = 0;
        for record in file.processes {
            match process_start_time(record.pid).await {
                Some(start_time) if start_time == record.start_time => {
                    // SAFETY: kill(2) with a negative PID signals the recorded
                    // process group, whose leader was just verified above.
                    let result = unsafe { libc::kill(-(record.pgid as i32), libc::SIGKILL) };
                    if result == 0 {
                        reaped += 1;
                        tracing::info!(pid = record.pid, program = %record.program, "killed orphaned provider process group");
                    } else {
                        tracing::warn!(
                            pid = record.pid,
                            error = %std::io::Error::last_os_error(),
                            "failed to kill orphaned provider process group"
                        );
                    }
                }
                _ => tracing::debug!(
                    pid = record.pid,
                    "provider process from a previous run already exited"
                ),
            }
        }
        Ok(reaped)
    }

    /// The process's start time as printed by `ps`, or `None` if it does not
    /// exist or `ps` is unavailable. Pinned to UTC and the C locale so the text
    /// is stable across daemon runs.
    pub(super) async fn process_start_time(pid: u32) -> Option<String> {
        let output = tokio::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let start_time = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (!start_time.is_empty()).then_some(start_time)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::time::Duration;

    use super::unix::{
        process_start_time, reap_orphans, ProcessRecord, ProcessRegistry, RegistryOwner,
    };

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn spawn_group_leader() -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn write_records(path: &std::path::Path, records: &[ProcessRecord]) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({ "schemaVersion": 1, "processes": records }))
                .unwrap(),
        )
        .unwrap();
    }

    async fn exited_within(child: &mut std::process::Child, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn orphan_with_matching_start_time_is_killed_and_a_mismatch_is_left_alone() {
        let root = temp_dir("todex-provider-orphans");
        let path = root.join("provider_processes.json");
        let mut orphan = spawn_group_leader();
        let mut stranger = spawn_group_leader();
        let orphan_start = process_start_time(orphan.id()).await.unwrap();
        write_records(
            &path,
            &[
                ProcessRecord {
                    pid: orphan.id(),
                    pgid: orphan.id(),
                    start_time: orphan_start,
                    program: "fixture".to_owned(),
                },
                ProcessRecord {
                    pid: stranger.id(),
                    pgid: stranger.id(),
                    // A reused PID: same number, different process.
                    start_time: "Thu Jan  1 00:00:00 1970".to_owned(),
                    program: "fixture".to_owned(),
                },
            ],
        );

        assert_eq!(reap_orphans(&path).await, Ok(1));
        assert!(exited_within(&mut orphan, Duration::from_secs(5)).await);
        assert!(stranger.try_wait().unwrap().is_none());

        stranger.kill().unwrap();
        stranger.wait().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tracked_processes_are_recorded_until_released() {
        let root = temp_dir("todex-provider-registry");
        let path = root.join("provider_processes.json");
        let registry = ProcessRegistry::new(path.clone(), None);
        let mut child = spawn_group_leader();
        let tracked = registry.clone().track(child.id(), "fixture").await.unwrap();
        let recorded: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(recorded["processes"][0]["pid"], child.id());
        assert_eq!(recorded["processes"][0]["pgid"], child.id());
        assert_eq!(
            recorded["processes"][0]["startTime"],
            process_start_time(child.id()).await.unwrap()
        );
        drop(tracked);
        let released: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(released["processes"], serde_json::json!([]));

        child.kill().unwrap();
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_live_owner_keeps_its_providers_from_a_second_server() {
        let root = temp_dir("todex-provider-owner");
        let path = root.join("provider_processes.json");
        let mut owner_process = spawn_group_leader();
        let mut provider = spawn_group_leader();
        let owner = RegistryOwner {
            pid: owner_process.id(),
            start_time: process_start_time(owner_process.id()).await.unwrap(),
        };
        let registry = ProcessRegistry::new(path.clone(), Some(owner.clone()));
        let tracked = registry
            .clone()
            .track(provider.id(), "fixture")
            .await
            .unwrap();

        assert_eq!(reap_orphans(&path).await, Err(owner));
        assert!(provider.try_wait().unwrap().is_none());

        // Once the owning server is gone its providers are orphans again.
        owner_process.kill().unwrap();
        owner_process.wait().unwrap();
        assert_eq!(reap_orphans(&path).await, Ok(1));
        assert!(exited_within(&mut provider, Duration::from_secs(5)).await);

        drop(tracked);
        let _ = std::fs::remove_dir_all(root);
    }
}
