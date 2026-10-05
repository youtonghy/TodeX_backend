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
    sync::{Arc, Mutex},
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
    /// Asks the person at this computer; `None` when nobody can be asked.
    async fn confirm(&self, title: String, message: String, timeout: Duration) -> Option<bool>;
    async fn observe(&self, args: Value) -> Result<Value, ComputerError>;
    /// `allowed_apps` and `confirmed` come from the user, never the agent.
    async fn act(
        &self,
        args: Value,
        allowed_apps: Vec<String>,
        confirmed: bool,
    ) -> Result<Value, ComputerError>;
    /// A JPEG of the controlled display for live viewers.
    async fn frame(&self, max_width: u32, quality: u8) -> Result<Vec<u8>, ComputerError>;
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

#[derive(Default)]
struct NativeComputer {
    engine: Arc<Mutex<Engine>>,
}

impl NativeComputer {
    async fn with_engine<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Engine) -> Result<T, ComputerError> + Send + 'static,
    ) -> Result<T, ComputerError> {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut engine = engine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&mut engine)
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
                Some("Screen Recording and Accessibility must be granted to the TodeX backend on this computer.".to_owned())
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
            let _ = tokio::task::spawn_blocking(platform::request_permissions).await;
        }
        self.status()
    }

    async fn confirm(&self, title: String, message: String, timeout: Duration) -> Option<bool> {
        tokio::task::spawn_blocking(move || host_ui::confirm(&title, &message, timeout))
            .await
            .ok()
            .flatten()
    }

    async fn observe(&self, args: Value) -> Result<Value, ComputerError> {
        self.with_engine(move |engine| engine.observe(&args)).await
    }

    async fn act(
        &self,
        args: Value,
        allowed_apps: Vec<String>,
        confirmed: bool,
    ) -> Result<Value, ComputerError> {
        self.with_engine(move |engine| {
            engine.act(
                &args,
                Grants {
                    allowed_apps: &allowed_apps,
                    confirmed,
                },
                std::process::id(),
            )
        })
        .await
    }

    async fn frame(&self, max_width: u32, quality: u8) -> Result<Vec<u8>, ComputerError> {
        self.with_engine(move |engine| engine.frame(max_width, quality))
            .await
    }

    fn session(&self, summary: Option<&str>) {
        match summary {
            Some(summary) => host_ui::show_status(summary),
            None => {
                host_ui::hide_status();
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
