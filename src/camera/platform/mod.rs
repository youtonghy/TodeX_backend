//! The host's cameras: AVFoundation on macOS; other platforms report the
//! camera as unsupported for now.

use std::sync::Arc;

use super::CameraHost;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub(super) fn native() -> Arc<dyn CameraHost> {
    Arc::new(macos::MacCameras)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn native() -> Arc<dyn CameraHost> {
    Arc::new(super::Unsupported)
}
