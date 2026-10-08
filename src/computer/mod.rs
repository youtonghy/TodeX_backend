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
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use self::{
    engine::{Engine, Grants},
    platform::Permissions,
    policy::PolicyFailure,
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

/// The host-facing half of Computer Use; tests replace it.
#[async_trait]
pub(crate) trait ComputerHost: Send + Sync {
    fn status(&self) -> ComputerStatus;
    async fn request_permissions(&self) -> ComputerStatus;
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

#[derive(Default)]
struct NativeComputer {
    engine: Arc<tokio::sync::Mutex<Engine>>,
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
        self.with_engine_within(ENGINE_WAIT, lease, work).await
    }

    async fn with_engine_within<T: Send + 'static>(
        &self,
        wait: Duration,
        lease: u64,
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
            engine.enter(lease);
            work(&mut engine, &abandoned)
        })
        .await
        .map_err(ComputerError::platform)?
    }
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

    async fn request_permissions(&self) -> ComputerStatus {
        if platform::unsupported_reason().is_none() {
            if let Err(error) = tokio::task::spawn_blocking(platform::request_permissions).await {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_call_waits_for_a_busy_engine_only_so_long() {
        let computer = Arc::new(NativeComputer::default());
        let first = {
            let computer = computer.clone();
            tokio::spawn(async move {
                computer
                    .with_engine_within(Duration::from_secs(5), 1, |_, _| {
                        std::thread::sleep(Duration::from_millis(400));
                        Ok("first")
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let busy = computer
            .with_engine_within(Duration::from_millis(50), 1, |_, _| Ok("second"))
            .await;
        assert_eq!(busy.unwrap_err().code, "BUSY");
        assert_eq!(first.await.unwrap().unwrap(), "first");
        // Once free, the next call runs.
        let after = computer
            .with_engine_within(Duration::from_millis(50), 1, |_, _| Ok("third"))
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
                    .with_engine_within(Duration::from_secs(5), 1, move |_, _| {
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
