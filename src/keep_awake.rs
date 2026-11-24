//! Keeps a macOS host awake while it serves remote clients.
//!
//! A Mac that idles into system sleep suspends the daemon: mesh relays stop
//! getting pongs and drop its uplinks, and clients reaching it through a
//! relay get 502s until it wakes. With `keep_awake` on (the default) and the
//! network listener on, the daemon holds an IOKit `PreventSystemSleep`
//! assertion. macOS honours that assertion on AC power only, so on battery
//! the Mac still sleeps; the display still sleeps either way. The kernel
//! drops the assertion when the process exits, so a crash cannot leave the
//! host stuck awake.
//!
//! The running daemon rereads `keep_awake` from `config.toml` every
//! [`RECHECK_INTERVAL`], so a change saved by the TUI applies without a
//! restart; the local admin API applies its change at once.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config::{Config, Deployment};

/// How often a running daemon rereads `keep_awake` from `config.toml`.
const RECHECK_INTERVAL: Duration = Duration::from_secs(15);

/// The assertion this process holds, if any.
static HELD: Mutex<Held> = Mutex::new(Held {
    assertion: None,
    failure_logged: false,
});

struct Held {
    assertion: Option<platform::Assertion>,
    /// Set after a failed acquire was logged, so the periodic recheck does
    /// not repeat the same warning; cleared on the next success.
    failure_logged: bool,
}

/// Whether this daemon can keep its host awake: macOS, and not a container
/// (the container's host decides its own power policy).
pub fn supported(config: &Config) -> bool {
    cfg!(target_os = "macos") && config.deployment.kind != Deployment::Container
}

/// Whether the assertion is held right now.
#[cfg(test)]
fn active() -> bool {
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .assertion
        .is_some()
}

/// Holds or releases the assertion for a `keep_awake` value of `enabled`.
/// Only a supported daemon with the network listener on keeps the host
/// awake: an IPC-only daemon serves nobody remote.
pub fn apply(config: &Config, enabled: bool) {
    set(supported(config) && config.network_enabled && enabled);
}

fn set(hold: bool) {
    let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    match (hold, held.assertion.is_some()) {
        (true, false) => match platform::Assertion::acquire() {
            Ok(assertion) => {
                held.assertion = Some(assertion);
                held.failure_logged = false;
                tracing::info!("keeping the host awake on AC power while serving remote clients");
            }
            Err(error) => {
                if !held.failure_logged {
                    held.failure_logged = true;
                    tracing::warn!(%error, "could not keep the host awake");
                }
            }
        },
        (false, true) => {
            held.assertion = None;
            tracing::info!("no longer keeping the host awake");
        }
        _ => {}
    }
}

/// Applies `config.keep_awake` and starts rereading it from `config.toml`.
/// `None` when the platform cannot keep the host awake. Call [`stop`] on
/// shutdown.
pub fn spawn(config: &Config) -> Option<JoinHandle<()>> {
    if !supported(config) {
        return None;
    }
    apply(config, config.keep_awake);
    let config = config.clone();
    Some(tokio::spawn(async move {
        let data_dir: PathBuf = config.data_dir.clone();
        let mut read_failed = false;
        let mut ticker = tokio::time::interval(RECHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match Config::saved_keep_awake(&data_dir) {
                Ok(enabled) => {
                    read_failed = false;
                    apply(&config, enabled);
                }
                // Keep the current state: a half-written or invalid file
                // should not flip the host's power policy.
                Err(error) if !read_failed => {
                    read_failed = true;
                    tracing::warn!(error = %error, "could not reread keep_awake from config.toml");
                }
                Err(_) => {}
            }
        }
    }))
}

/// Stops the recheck task and releases the assertion.
pub fn stop(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
    set(false);
}

#[cfg(all(target_os = "macos", not(test)))]
mod platform {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    /// `kIOPMAssertionLevelOn`.
    const ASSERTION_LEVEL_ON: u32 = 255;
    /// `kIOReturnSuccess`.
    const IO_RETURN_SUCCESS: i32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: u32,
            name: CFStringRef,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }

    /// A held `PreventSystemSleep` assertion, released on drop.
    pub(super) struct Assertion(u32);

    impl Assertion {
        pub(super) fn acquire() -> Result<Self, String> {
            // `kIOPMAssertionTypePreventSystemSleep`: in effect on AC power only.
            let kind = CFString::from_static_string("PreventSystemSleep");
            let name = CFString::from_static_string("TodeX backend is serving remote clients");
            let mut id = 0u32;
            // SAFETY: both strings are live CFStrings for the whole call and
            // `id` is a valid place for IOKit to write the assertion id.
            let status = unsafe {
                IOPMAssertionCreateWithName(
                    kind.as_concrete_TypeRef(),
                    ASSERTION_LEVEL_ON,
                    name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if status == IO_RETURN_SUCCESS {
                Ok(Self(id))
            } else {
                Err(format!("IOPMAssertionCreateWithName returned {status:#x}"))
            }
        }
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful create and is released
            // exactly once, here.
            let status = unsafe { IOPMAssertionRelease(self.0) };
            if status != IO_RETURN_SUCCESS {
                tracing::warn!(status, "IOPMAssertionRelease failed");
            }
        }
    }
}

/// Elsewhere (and in tests, which must not change the host's power policy)
/// the assertion is a marker; [`supported`] keeps it off other platforms.
#[cfg(any(not(target_os = "macos"), test))]
mod platform {
    pub(super) struct Assertion;

    impl Assertion {
        pub(super) fn acquire() -> Result<Self, String> {
            Ok(Self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_only_when_supported_networked_and_enabled() {
        let mut config = Config::default();
        config.deployment.kind = Deployment::Host;
        config.network_enabled = true;

        apply(&config, true);
        assert_eq!(active(), cfg!(target_os = "macos"));
        apply(&config, true);
        assert_eq!(active(), cfg!(target_os = "macos"), "holding is idempotent");
        apply(&config, false);
        assert!(!active());

        config.network_enabled = false;
        apply(&config, true);
        assert!(!active(), "an IPC-only daemon lets the host sleep");

        config.network_enabled = true;
        config.deployment.kind = Deployment::Container;
        apply(&config, true);
        assert!(!active(), "a container leaves power to its host");
        assert!(!supported(&config));

        config.deployment.kind = Deployment::Host;
        apply(&config, true);
        stop(None);
        assert!(!active(), "stop releases the assertion");
    }
}
