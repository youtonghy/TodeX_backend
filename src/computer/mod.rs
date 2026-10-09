//! Computer Use on the daemon's own host: agents observe a window (an
//! accessibility tree with refs plus a screenshot) and act on it, through
//! [xa11y](https://github.com/xa11y/xa11y) in this process. Clients only
//! watch (frames, screenshots, the action journal) and answer prompts.
//!
//! The person at the host stays in control: the first grant of each
//! conversation is confirmed on this computer ([`host_ui::confirm`]), a
//! pill and a global shortcut stop the session, and [`policy`] keeps
//! agents out of credential stores and TodeX itself.

mod engine;
pub(crate) mod host_ui;
pub(crate) mod keys;
pub(crate) mod platform;
pub(crate) mod policy;
pub(crate) mod tree;

use std::{
    collections::HashSet,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use self::{
    engine::{Engine, Grants},
    platform::Permissions,
    policy::{PolicyFailure, Target},
};

/// A refused or failed Computer Use call, reported as `CODE: message`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ComputerError {
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
}

impl ComputerError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            detail: None,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message)
    }

    pub(crate) fn platform(error: impl fmt::Display) -> Self {
        Self::new("EXECUTOR_FAILED", error.to_string())
    }
}

impl fmt::Display for ComputerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<PolicyFailure> for ComputerError {
    fn from(failure: PolicyFailure) -> Self {
        Self {
            code: failure.code.to_owned(),
            message: failure.message,
            detail: failure.detail,
        }
    }
}

/// Whether Computer Use can run on this host, for settings screens.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ComputerStatus {
    /// The OS and session can run it at all.
    pub supported: bool,
    /// Supported, permitted, and someone at the host can confirm grants.
    pub available: bool,
    /// Why it is not available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The computer agents would control.
    pub host: String,
    pub platform: &'static str,
    pub permissions: Permissions,
}

/// An app on this host that agents can name, for the composer's `@app:`
/// mentions. `id` is what `computer_act`'s `open_app` and the `app`
/// parameters accept.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedApp {
    pub id: String,
    pub name: String,
    pub running: bool,
}

/// The host-facing half of Computer Use; tests replace it.
#[async_trait]
pub(crate) trait ComputerHost: Send + Sync {
    fn status(&self) -> ComputerStatus;
    /// `which`: one permission, or `None` for every missing one.
    async fn request_permissions(&self, which: Option<platform::Permission>) -> ComputerStatus;
    /// Asks the person at this computer: their answer, `Ok(None)` when
    /// nobody can be asked (no host UI or dialog tool), `Err` when asking
    /// failed.
    async fn confirm(
        &self,
        title: String,
        message: String,
        timeout: Duration,
    ) -> Result<Option<bool>, ComputerError>;
    /// `lease` identifies the screen lease the call belongs to: refs and
    /// the screenshot mapping of another lease are forgotten.
    async fn observe(&self, lease: u64, args: Value) -> Result<Value, ComputerError>;
    /// `allowed_apps` and `confirmed` come from the user, never the agent.
    async fn act(
        &self,
        lease: u64,
        args: Value,
        allowed_apps: Vec<String>,
        confirmed: bool,
    ) -> Result<Value, ComputerError>;
    /// A JPEG of the controlled display for live viewers.
    async fn frame(
        &self,
        lease: u64,
        max_width: u32,
        quality: u8,
    ) -> Result<Vec<u8>, ComputerError>;
    /// A conversation took the screen (`Some`) or gave it back (`None`).
    fn session(&self, summary: Option<&str>);
    /// Apps agents may name on this host: running ones first, then
    /// installed ones; protected apps left out (see [`app_list`]).
    async fn apps(&self) -> Result<Vec<ListedApp>, ComputerError> {
        Ok(Vec::new())
    }
}

/// Computer Use for this daemon.
#[derive(Clone)]
pub(crate) struct Computer(Arc<dyn ComputerHost>);

impl Computer {
    pub(crate) fn native() -> Self {
        Self(Arc::new(NativeComputer::default()))
    }

    #[cfg(test)]
    pub(crate) fn with_host(host: Arc<dyn ComputerHost>) -> Self {
        Self(host)
    }

    pub(crate) fn host(&self) -> &dyn ComputerHost {
        self.0.as_ref()
    }
}

/// How long a call waits for the engine while an earlier call (one whose
/// caller gave up included) still holds it.
const ENGINE_WAIT: Duration = Duration::from_secs(10);
/// Listing apps waits less: while an action holds the engine the list
/// goes out without running apps rather than keep the composer waiting.
const APPS_ENGINE_WAIT: Duration = Duration::from_secs(2);
/// How long the scan of installed apps is reused.
const INSTALLED_APPS_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct NativeComputer {
    engine: Arc<tokio::sync::Mutex<Engine>>,
    /// The latest scan of installed apps. Held across the scan, so
    /// concurrent listings share one.
    installed: tokio::sync::Mutex<Option<(Instant, Arc<Vec<Target>>)>>,
}

/// Marks the work of a call abandoned when its future is dropped (the
/// caller timed out or was cancelled), so the blocking thread, which
/// cannot be stopped, sends no input it has not sent yet.
struct AbandonOnDrop(Arc<AtomicBool>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl NativeComputer {
    async fn with_engine<T: Send + 'static>(
        &self,
        lease: u64,
        work: impl FnOnce(&mut Engine, &AtomicBool) -> Result<T, ComputerError> + Send + 'static,
    ) -> Result<T, ComputerError> {
        self.with_engine_within(ENGINE_WAIT, Some(lease), work)
            .await
    }

    /// `lease`: the screen lease the call belongs to; `None` for calls
    /// outside any (listing apps), which leave the observation alone.
    async fn with_engine_within<T: Send + 'static>(
        &self,
        wait: Duration,
        lease: Option<u64>,
        work: impl FnOnce(&mut Engine, &AtomicBool) -> Result<T, ComputerError> + Send + 'static,
    ) -> Result<T, ComputerError> {
        let abandoned = Arc::new(AtomicBool::new(false));
        let _abandon = AbandonOnDrop(abandoned.clone());
        // Waiting here rather than on a blocking thread keeps calls that
        // queue behind a slow one from exhausting the blocking pool.
        let mut engine = tokio::time::timeout(wait, self.engine.clone().lock_owned())
            .await
            .map_err(|_| {
                ComputerError::new(
                    "BUSY",
                    "an earlier Computer Use call is still running on this computer; retry shortly",
                )
            })?;
        tokio::task::spawn_blocking(move || {
            // The caller may have given up while the engine was taken.
            if abandoned.load(Ordering::SeqCst) {
                return Err(ComputerError::new("CANCELLED", "the call was abandoned"));
            }
            if let Some(lease) = lease {
                engine.enter(lease);
            }
            work(&mut engine, &abandoned)
        })
        .await
        .map_err(ComputerError::platform)?
    }

    async fn installed_apps(&self) -> Result<Arc<Vec<Target>>, ComputerError> {
        let mut cached = self.installed.lock().await;
        if let Some((scanned_at, apps)) = cached.as_ref() {
            if scanned_at.elapsed() < INSTALLED_APPS_TTL {
                return Ok(apps.clone());
            }
        }
        let apps = Arc::new(
            tokio::task::spawn_blocking(platform::installed_apps)
                .await
                .map_err(ComputerError::platform)?,
        );
        *cached = Some((Instant::now(), apps.clone()));
        Ok(apps)
    }
}

/// The `@app:` list: `running` (front app `front_pid` first) then
/// `installed`, each sorted by name, one entry per id (case-insensitive,
/// running wins). Ids that are empty, contain whitespace (a mention ends
/// at whitespace) or name a blocked app are left out.
fn app_list(running: Vec<Target>, front_pid: Option<u32>, installed: &[Target]) -> Vec<ListedApp> {
    let named = |app: Target| Target {
        name: match app.name.trim() {
            "" => app.id.clone(),
            name => name.to_owned(),
        },
        ..app
    };
    let by_name = |a: &Target, b: &Target| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    };
    let front = |app: &Target| front_pid.is_some_and(|pid| pid != 0 && app.pid == pid);
    let mut running: Vec<Target> = running.into_iter().map(named).collect();
    running.sort_by(|a, b| front(b).cmp(&front(a)).then_with(|| by_name(a, b)));
    let mut installed: Vec<Target> = installed.iter().cloned().map(named).collect();
    installed.sort_by(by_name);
    let mut seen = HashSet::new();
    running
        .into_iter()
        .map(|app| (app, true))
        .chain(installed.into_iter().map(|app| (app, false)))
        .filter(|(app, _)| {
            !app.id.is_empty()
                && !app.id.contains(char::is_whitespace)
                && !policy::is_blocked(&app.id)
                && seen.insert(app.id.to_lowercase())
        })
        .map(|(app, running)| ListedApp {
            id: app.id,
            name: app.name,
            running,
        })
        .collect()
}

#[async_trait]
impl ComputerHost for NativeComputer {
    fn status(&self) -> ComputerStatus {
        let unsupported = platform::unsupported_reason();
        let permissions = if unsupported.is_some() {
            Permissions::default()
        } else {
            platform::permissions()
        };
        let reason = unsupported.clone().or_else(|| {
            if !permissions.all() {
                Some(platform::missing_permissions_reason(permissions))
            } else if !host_ui::available() {
                Some("Run the TodeX backend as a service or with `todex-agentd serve` on this computer's desktop session.".to_owned())
            } else {
                None
            }
        });
        ComputerStatus {
            supported: unsupported.is_none(),
            available: reason.is_none(),
            reason,
            host: host_name(),
            platform: std::env::consts::OS,
            permissions,
        }
    }

    async fn request_permissions(&self, which: Option<platform::Permission>) -> ComputerStatus {
        if platform::unsupported_reason().is_none() {
            if let Err(error) =
                tokio::task::spawn_blocking(move || platform::request_permissions(which)).await
            {
                tracing::warn!(%error, "requesting Computer Use permissions failed");
            }
        }
        self.status()
    }

    async fn confirm(
        &self,
        title: String,
        message: String,
        timeout: Duration,
    ) -> Result<Option<bool>, ComputerError> {
        tokio::task::spawn_blocking(move || host_ui::confirm(&title, &message, timeout))
            .await
            .map_err(ComputerError::platform)
    }

    async fn observe(&self, lease: u64, args: Value) -> Result<Value, ComputerError> {
        self.with_engine(lease, move |engine, _| {
            engine.observe(&args, std::process::id())
        })
        .await
    }

    async fn act(
        &self,
        lease: u64,
        args: Value,
        allowed_apps: Vec<String>,
        confirmed: bool,
    ) -> Result<Value, ComputerError> {
        self.with_engine(lease, move |engine, cancelled| {
            engine.act(
                &args,
                Grants {
                    allowed_apps: &allowed_apps,
                    confirmed,
                    cancelled,
                },
                std::process::id(),
            )
        })
        .await
    }

    async fn frame(
        &self,
        lease: u64,
        max_width: u32,
        quality: u8,
    ) -> Result<Vec<u8>, ComputerError> {
        self.with_engine(lease, move |engine, _| {
            engine.frame(max_width, quality, std::process::id())
        })
        .await
    }

    async fn apps(&self) -> Result<Vec<ListedApp>, ComputerError> {
        let (running, front) = self
            .with_engine_within(APPS_ENGINE_WAIT, None, |engine, _| {
                engine.running_apps(std::process::id())
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "listing running apps failed; listing installed apps only");
                (Vec::new(), None)
            });
        let installed = self.installed_apps().await?;
        Ok(app_list(running, front, &installed))
    }

    fn session(&self, summary: Option<&str>) {
        match summary {
            Some(summary) => {
                host_ui::show_status(summary);
                platform::begin_session();
            }
            None => {
                host_ui::hide_status();
                platform::end_session();
                // The next conversation starts from a fresh observation.
                if let Ok(mut engine) = self.engine.try_lock() {
                    engine.reset();
                }
            }
        }
    }
}

/// This computer's name as people know it.
pub(crate) fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is writable for its full length.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
            return name.trim_end_matches(".local").to_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, pid: u32) -> Target {
        Target {
            id: id.into(),
            name: name.into(),
            pid,
        }
    }

    #[test]
    fn the_app_list_puts_running_apps_first_once_each() {
        // An id of every platform's blocked list.
        let blocked = if cfg!(target_os = "macos") {
            "com.apple.keychainaccess"
        } else if cfg!(target_os = "windows") {
            "regedit.exe"
        } else {
            "seahorse"
        };
        let running = vec![
            app("org.zed", "zed", 3),
            app("com.Apple.Notes", "Notes", 2),
            app("org.front", "Zebra", 9),
            app(blocked, "Locked", 4),
        ];
        let installed = [
            app("com.apple.notes", "Notes", 0),
            app("com.apple.Calculator", "Calculator", 0),
            app("", "No id", 0),
            app("has space", "Spaced", 0),
            app("org.unnamed", " ", 0),
            app(&blocked.to_uppercase(), "Locked", 0),
        ];
        let listed: Vec<(String, String, bool)> = app_list(running, Some(9), &installed)
            .into_iter()
            .map(|app| (app.id, app.name, app.running))
            .collect();
        let expected = [
            // The front app, then running apps by name.
            ("org.front", "Zebra", true),
            ("com.Apple.Notes", "Notes", true),
            ("org.zed", "zed", true),
            // Installed ones by name; Notes already listed as running.
            ("com.apple.Calculator", "Calculator", false),
            ("org.unnamed", "org.unnamed", false),
        ]
        .map(|(id, name, running)| (id.to_owned(), name.to_owned(), running));
        assert_eq!(listed, expected);
    }

    #[tokio::test]
    #[ignore = "lists this computer's apps; run with --ignored to see them and the timing"]
    async fn the_native_app_list_runs_on_this_host() {
        let computer = NativeComputer::default();
        for round in ["cold", "cached"] {
            let started = Instant::now();
            let apps = computer.apps().await.unwrap();
            let running = apps.iter().filter(|app| app.running).count();
            eprintln!(
                "{round}: {} apps ({running} running) in {:?}",
                apps.len(),
                started.elapsed()
            );
            for app in apps.iter().take(5) {
                eprintln!("  {app:?}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_call_waits_for_a_busy_engine_only_so_long() {
        let computer = Arc::new(NativeComputer::default());
        let first = {
            let computer = computer.clone();
            tokio::spawn(async move {
                computer
                    .with_engine_within(Duration::from_secs(5), Some(1), |_, _| {
                        std::thread::sleep(Duration::from_millis(400));
                        Ok("first")
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let busy = computer
            .with_engine_within(Duration::from_millis(50), Some(1), |_, _| Ok("second"))
            .await;
        assert_eq!(busy.unwrap_err().code, "BUSY");
        assert_eq!(first.await.unwrap().unwrap(), "first");
        // Once free, the next call runs.
        let after = computer
            .with_engine_within(Duration::from_millis(50), Some(1), |_, _| Ok("third"))
            .await;
        assert_eq!(after.unwrap(), "third");
    }

    #[tokio::test]
    async fn a_call_dropped_while_waiting_never_runs() {
        let computer = Arc::new(NativeComputer::default());
        let ran = Arc::new(AtomicBool::new(false));
        let guard = computer.engine.clone().lock_owned().await;
        let waiting = {
            let (computer, ran) = (computer.clone(), ran.clone());
            tokio::spawn(async move {
                computer
                    .with_engine_within(Duration::from_secs(5), Some(1), move |_, _| {
                        ran.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        waiting.abort();
        drop(guard);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!ran.load(Ordering::SeqCst));
    }
}
