//! Provider processes run in their own process group, so a daemon that dies
//! without running destructors (SIGKILL, OOM, power loss) leaves them running.
//! While a server is active, every provider it spawns is recorded in
//! `<data_dir>/provider_processes.json`; the next start kills each recorded
//! group whose leader is still the recorded process, identified by PID *and*
//! start time so a reused PID is never signalled (read from the kernel on
//! macOS and Linux; `ps` elsewhere and for records of older daemons). The file
//! is rewritten off the async runtime, newest snapshot last. It also names the
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
        /// [`start_time`]: kernel-reported where available, else
        /// `ps -o lstart=` in UTC; compared verbatim.
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
        records: Mutex<Records>,
        /// Generation of the snapshot last written; writers hold it, so an
        /// older snapshot never replaces a newer one.
        written: Mutex<u64>,
    }

    #[derive(Default)]
    struct Records {
        /// Bumped on every change.
        generation: u64,
        list: Vec<ProcessRecord>,
    }

    type Snapshot = (u64, Vec<ProcessRecord>);

    /// Removes its record when the provider process has been reaped or killed.
    pub(crate) struct TrackedProcess {
        registry: Arc<ProcessRegistry>,
        pid: u32,
    }

    impl Drop for TrackedProcess {
        fn drop(&mut self) {
            let Some(snapshot) = self.registry.edit(|records| {
                let before = records.len();
                records.retain(|record| record.pid != self.pid);
                records.len() != before
            }) else {
                return;
            };
            let registry = self.registry.clone();
            // Dropped on a runtime thread when a provider exits: write the
            // file from the blocking pool instead of stalling the runtime.
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn_blocking(move || registry.write(snapshot));
                }
                Err(_) => registry.write(snapshot),
            }
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
            records: Mutex::new(Records::default()),
            written: Mutex::new(0),
        });
        let initial = registry.clone();
        if let Err(error) =
            tokio::task::spawn_blocking(move || initial.write((0, Vec::new()))).await
        {
            tracing::warn!(error = %error, "failed to reset the provider process registry");
        }
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
                records: Mutex::new(Records::default()),
                written: Mutex::new(0),
            })
        }

        pub(super) async fn track(
            self: Arc<Self>,
            pid: u32,
            program: &str,
        ) -> Option<TrackedProcess> {
            let Some(start_time) = start_time(pid).await else {
                tracing::warn!(
                    pid,
                    "could not read the provider's start time; it will not be reaped after a crash"
                );
                return None;
            };
            let snapshot = self.edit(|records| {
                records.retain(|record| record.pid != pid);
                records.push(ProcessRecord {
                    pid,
                    // Providers are spawned with process_group(0).
                    pgid: pid,
                    start_time,
                    program: program.to_owned(),
                });
                true
            });
            if let Some(snapshot) = snapshot {
                let registry = self.clone();
                if let Err(error) =
                    tokio::task::spawn_blocking(move || registry.write(snapshot)).await
                {
                    tracing::warn!(error = %error, "failed to persist the provider process registry");
                }
            }
            Some(TrackedProcess {
                registry: self,
                pid,
            })
        }

        /// Applies `change` to the records; returns the new snapshot when it
        /// changed anything.
        fn edit(&self, change: impl FnOnce(&mut Vec<ProcessRecord>) -> bool) -> Option<Snapshot> {
            let mut records = self
                .records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !change(&mut records.list) {
                return None;
            }
            records.generation += 1;
            Some((records.generation, records.list.clone()))
        }

        /// Writes `snapshot` unless a newer one was written already. Blocking:
        /// call from the blocking pool. Failing to persist only weakens crash
        /// cleanup; the provider itself is unaffected, so the error is logged
        /// rather than propagated.
        fn write(&self, (generation, records): Snapshot) {
            let mut written = self
                .written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if generation < *written {
                return;
            }
            let file = RegistryFile {
                schema_version: SCHEMA_VERSION,
                owner: self.owner.clone(),
                processes: records,
            };
            if let Err(error) = write_private_atomic(&self.path, &file) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "failed to persist the provider process registry"
                );
            }
            *written = generation;
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
        let start_time = start_time(pid).await?;
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
                && start_time_matches(owner.pid, &owner.start_time).await;
            if alive {
                return Err(owner);
            }
        }
        let mut reaped = 0;
        for record in file.processes {
            match start_time_matches(record.pid, &record.start_time).await {
                true => {
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
                false => tracing::debug!(
                    pid = record.pid,
                    "provider process from a previous run already exited"
                ),
            }
        }
        Ok(reaped)
    }

    /// Prefix of start times read from the kernel; others came from `ps`.
    const KERNEL_START_TIME: &str = "kernel:";

    /// The process's start time, or `None` if it does not exist. Read from
    /// the kernel where possible: spawning `ps` for every provider launch
    /// costs a process per spawn.
    pub(super) async fn start_time(pid: u32) -> Option<String> {
        match kernel_start_time(pid) {
            Some(start_time) => Some(start_time),
            None if cfg!(any(target_os = "macos", target_os = "linux")) => None,
            None => process_start_time(pid).await,
        }
    }

    /// Whether `pid` is still the process recorded with `recorded`, in
    /// whichever format that record was written.
    async fn start_time_matches(pid: u32, recorded: &str) -> bool {
        if recorded.starts_with(KERNEL_START_TIME) {
            kernel_start_time(pid).as_deref() == Some(recorded)
        } else {
            process_start_time(pid).await.as_deref() == Some(recorded)
        }
    }

    #[cfg(target_os = "macos")]
    fn kernel_start_time(pid: u32) -> Option<String> {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: proc_bsdinfo is plain old data; proc_pidinfo fills at most
        // `size` bytes of it and reports how many it wrote.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (written == size).then(|| {
            format!(
                "{KERNEL_START_TIME}{}.{:06}",
                info.pbi_start_tvsec, info.pbi_start_tvusec
            )
        })
    }

    /// Field 22 of `/proc/<pid>/stat` (start time in clock ticks since boot),
    /// qualified by the boot id so it cannot match after a reboot.
    #[cfg(target_os = "linux")]
    fn kernel_start_time(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may contain spaces and parentheses; fields resume
        // after the last ')', starting with field 3 (state).
        let ticks = stat
            .get(stat.rfind(')')? + 1..)?
            .split_whitespace()
            .nth(19)?;
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("{KERNEL_START_TIME}{}:{ticks}", boot_id.trim()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn kernel_start_time(_pid: u32) -> Option<String> {
        None
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
        process_start_time, reap_orphans, start_time, ProcessRecord, ProcessRegistry, RegistryOwner,
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
    async fn kernel_start_times_identify_live_processes_without_ps() {
        let mut child = spawn_group_leader();
        let recorded = start_time(child.id()).await.unwrap();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(recorded.starts_with("kernel:"), "{recorded}");
        }
        assert_eq!(
            start_time(child.id()).await.as_deref(),
            Some(recorded.as_str())
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(start_time(child.id()).await, None);
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
            start_time(child.id()).await.unwrap()
        );
        drop(tracked);
        // The release is written from the blocking pool.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let released: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if released["processes"] == serde_json::json!([]) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{released}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

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
            start_time: start_time(owner_process.id()).await.unwrap(),
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
