//! Cameras through AVFoundation (`macos_camera.m`).
//!
//! macOS asks for camera access on behalf of the TCC-responsible process,
//! which is the daemon itself (see
//! `computer::platform::adopt_own_permission_identity`). Its executable
//! embeds an Info.plist with `NSCameraUsageDescription` and is signed with
//! the `com.apple.security.device.camera` entitlement (hardened runtime);
//! without either, macOS refuses without asking.

use std::{
    ffi::{c_char, c_void, CStr, CString},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;

use super::super::{CameraDevice, CameraError, CameraHost, CameraPermission, CameraStream, Frame};

/// How long the OS prompt waits for the person at the host.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(120);

unsafe extern "C" {
    fn todex_camera_devices() -> *mut c_char;
    fn todex_camera_authorization() -> i32;
    fn todex_camera_request_access(timeout_seconds: f64) -> i32;
    fn todex_camera_open(
        device_id: *const c_char,
        max_width: u32,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn todex_camera_copy_frame(
        handle: *mut c_void,
        seq: *mut u64,
        pixels: *mut *mut u8,
        width: *mut u32,
        height: *mut u32,
    ) -> i32;
    fn todex_camera_close(handle: *mut c_void);
    fn todex_camera_free(memory: *mut c_void);
}

fn permission(status: i32) -> CameraPermission {
    match status {
        3 => CameraPermission::Granted,
        2 => CameraPermission::Denied,
        1 => CameraPermission::Restricted,
        _ => CameraPermission::NotDetermined,
    }
}

pub(super) struct MacCameras;

#[async_trait]
impl CameraHost for MacCameras {
    fn unsupported_reason(&self) -> Option<String> {
        None
    }

    fn permission(&self) -> CameraPermission {
        // SAFETY: no arguments; reads the authorization status.
        permission(unsafe { todex_camera_authorization() })
    }

    async fn request_permission(&self) -> CameraPermission {
        let timeout = PERMISSION_TIMEOUT.as_secs_f64();
        // SAFETY: blocks this worker thread until answered or timed out.
        tokio::task::spawn_blocking(move || {
            permission(unsafe { todex_camera_request_access(timeout) })
        })
        .await
        .unwrap_or(CameraPermission::NotDetermined)
    }

    async fn devices(&self) -> Result<Vec<CameraDevice>, CameraError> {
        tokio::task::spawn_blocking(|| {
            // SAFETY: returns a malloc'd NUL-terminated string or NULL; it
            // is copied, then freed with the matching function.
            let raw = unsafe { todex_camera_devices() };
            if raw.is_null() {
                return Err(CameraError::new("FAILED", "cannot list the cameras"));
            }
            let json = unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned();
            unsafe { todex_camera_free(raw.cast()) };
            serde_json::from_str(&json).map_err(|error| {
                CameraError::new("FAILED", format!("cannot list the cameras: {error}"))
            })
        })
        .await
        .map_err(|error| CameraError::new("FAILED", error.to_string()))?
    }

    async fn open(
        &self,
        device_id: &str,
        max_width: u32,
    ) -> Result<Arc<dyn CameraStream>, CameraError> {
        let id = CString::new(device_id)
            .map_err(|_| CameraError::new("INVALID_ARGUMENT", "bad camera id"))?;
        tokio::task::spawn_blocking(move || {
            let mut error = [0 as c_char; 512];
            // SAFETY: `id` and `error` outlive the call; on success the
            // handle stays valid until `todex_camera_close`.
            let handle = unsafe {
                todex_camera_open(id.as_ptr(), max_width, error.as_mut_ptr(), error.len())
            };
            if handle.is_null() {
                let message = unsafe { CStr::from_ptr(error.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                return Err(CameraError::new("UNAVAILABLE", message));
            }
            Ok(Arc::new(MacStream { handle }) as Arc<dyn CameraStream>)
        })
        .await
        .map_err(|error| CameraError::new("FAILED", error.to_string()))?
    }
}

struct MacStream {
    handle: *mut c_void,
}

// SAFETY: the Objective-C side guards the newest frame with a lock; the
// handle is only closed on drop, when no other reference exists.
unsafe impl Send for MacStream {}
unsafe impl Sync for MacStream {}

impl CameraStream for MacStream {
    fn latest(&self, since: u64) -> Result<Option<(u64, Frame)>, CameraError> {
        let mut seq = since;
        let mut pixels: *mut u8 = std::ptr::null_mut();
        let (mut width, mut height) = (0u32, 0u32);
        // SAFETY: the out-pointers are valid; on 1 `pixels` holds
        // width * height * 4 malloc'd bytes, copied then freed.
        let result = unsafe {
            todex_camera_copy_frame(self.handle, &mut seq, &mut pixels, &mut width, &mut height)
        };
        match result {
            1 => {
                let len = width as usize * height as usize * 4;
                let bgra = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
                unsafe { todex_camera_free(pixels.cast()) };
                Ok(Some((
                    seq,
                    Frame {
                        width,
                        height,
                        bgra,
                    },
                )))
            }
            0 => Ok(None),
            _ => Err(CameraError::new(
                "FAILED",
                "cannot read the camera's picture",
            )),
        }
    }
}

impl Drop for MacStream {
    fn drop(&mut self) {
        // SAFETY: the handle came from `todex_camera_open` and is closed
        // once.
        unsafe { todex_camera_close(self.handle) };
    }
}
