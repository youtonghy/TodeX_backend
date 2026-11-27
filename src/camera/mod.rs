//! The agent camera: cameras on the daemon's own host that agents may use
//! through the `camera_*` tools of `todex_desktop`
//! ([`crate::agent_mcp`]), one approved session at a time.
//!
//! The person allows cameras one by one in settings ([`CameraSettings`]).
//! A session samples its camera, measures how much the picture changes
//! ([`engine`]) and keeps frames as JPEG files with their time in the name,
//! plus a `manifest.json`, in a directory the agent can read
//! ([`session`]). Agents get paths, never inline images.
//!
//! An open camera is shared: a session and any number of previews (the
//! approval card, the live view, settings) read its newest frame. A camera
//! nobody uses or watched for [`PREVIEW_IDLE`] is closed, so its light goes
//! out.
//!
//! Grants and sessions live in memory: a daemon restart asks again.

pub(crate) mod engine;
mod platform;
mod session;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::{
    agent_desktop::KeyedLocks, error::AppError, provider::ConversationSupervisor, secure_fs,
};

use engine::CaptureSettings;
pub(crate) use session::{Session, SessionEnd, Summary};

const SETTINGS_FILE: &str = "agent-camera.json";
/// A camera nobody uses or watched for this long is closed.
pub(crate) const PREVIEW_IDLE: Duration = Duration::from_secs(3);
/// Ended sessions can be collected (`camera_stop`) this long.
pub(crate) const RESULT_TTL: Duration = Duration::from_secs(30 * 60);
/// Frames are kept at most this wide.
pub(crate) const FRAME_MAX_WIDTH: u32 = 1280;
pub(crate) const FRAME_QUALITY: u8 = 70;
/// Previews (cards, live view, settings).
const PREVIEW_MAX_WIDTH: u32 = 640;
const PREVIEW_QUALITY: u8 = 60;
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const DEFAULT_MAX_SECONDS: u32 = 600;
pub(crate) const MAX_SECONDS_RANGE: (u32, u32) = (10, 3600);
pub(crate) const DEFAULT_MAX_FRAMES: u32 = 600;
pub(crate) const MAX_FRAMES_RANGE: (u32, u32) = (10, 2000);

/// A refused or failed camera call, reported as `CODE: message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CameraError {
    pub code: &'static str,
    pub message: String,
}

impl CameraError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CameraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CameraDevice {
    pub id: String,
    pub name: String,
}

/// The OS permission for the daemon to use cameras.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
// Only the macOS host reports every state; other hosts are unsupported.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum CameraPermission {
    Granted,
    Denied,
    /// Blocked by policy (MDM, parental controls).
    Restricted,
    /// Never asked: the person at the host is asked on request.
    NotDetermined,
}

/// One frame, tightly packed BGRA.
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// An open camera.
pub(crate) trait CameraStream: Send + Sync {
    /// The newest frame when it is newer than `since` (0: any), with its
    /// sequence number.
    fn latest(&self, since: u64) -> Result<Option<(u64, Frame)>, CameraError>;
}

/// The host's cameras; tests replace it.
#[async_trait]
pub(crate) trait CameraHost: Send + Sync {
    /// Why cameras cannot be used on this host at all, if they cannot.
    fn unsupported_reason(&self) -> Option<String>;
    fn permission(&self) -> CameraPermission;
    /// Shows the OS prompt on the host when undecided; returns the result.
    async fn request_permission(&self) -> CameraPermission;
    async fn devices(&self) -> Result<Vec<CameraDevice>, CameraError>;
    /// Starts the camera, delivering frames at most `max_width` wide.
    async fn open(
        &self,
        device_id: &str,
        max_width: u32,
    ) -> Result<Arc<dyn CameraStream>, CameraError>;
}

/// A camera the person lets agents use.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AllowedCamera {
    pub id: String,
    /// As last seen, for listing it while unplugged.
    pub name: String,
}

/// [`SETTINGS_FILE`]. Unknown fields are ignored, for newer daemons' files.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CameraSettings {
    /// Agents get the `camera_*` tools. Off by default; only effective while
    /// desktop tools are on.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed: Vec<AllowedCamera>,
    /// Offered first; one of `allowed`.
    #[serde(default)]
    pub default_device: Option<String>,
    /// What approval cards start with.
    #[serde(default)]
    pub capture: CaptureSettings,
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u32,
    #[serde(default = "default_max_frames")]
    pub max_frames: u32,
}

fn default_max_seconds() -> u32 {
    DEFAULT_MAX_SECONDS
}

fn default_max_frames() -> u32 {
    DEFAULT_MAX_FRAMES
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed: Vec::new(),
            default_device: None,
            capture: CaptureSettings::default(),
            max_seconds: DEFAULT_MAX_SECONDS,
            max_frames: DEFAULT_MAX_FRAMES,
        }
    }
}

impl CameraSettings {
    pub(crate) fn allows(&self, device_id: &str) -> bool {
        self.allowed.iter().any(|camera| camera.id == device_id)
    }

    /// The default camera, else the first allowed one.
    pub(crate) fn preferred(&self) -> Option<&AllowedCamera> {
        self.default_device
            .as_deref()
            .and_then(|id| self.allowed.iter().find(|camera| camera.id == id))
            .or_else(|| self.allowed.first())
    }
}

/// `PUT /v2/agent-desktop/camera`; absent fields stay.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SettingsUpdate {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Device ids, replacing the list.
    #[serde(default)]
    pub allowed: Option<Vec<String>>,
    /// A device id, or `null` for none.
    #[serde(default)]
    pub default_device: Option<Value>,
    #[serde(default)]
    pub capture: Option<CaptureSettings>,
    #[serde(default)]
    pub max_seconds: Option<u32>,
    #[serde(default)]
    pub max_frames: Option<u32>,
}

/// What a conversation may use without asking again ("allow for this
/// conversation").
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CameraGrant {
    pub device: CameraDevice,
    pub capture: CaptureSettings,
}

struct StreamEntry {
    stream: Arc<dyn CameraStream>,
    /// Sessions reading it.
    users: usize,
    last_viewed: Instant,
    preview: Option<(Instant, Arc<Vec<u8>>)>,
}

/// Daemon-wide camera state.
#[derive(Clone)]
pub struct AgentCamera {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    settings: RwLock<CameraSettings>,
    host: std::sync::RwLock<Arc<dyn CameraHost>>,
    streams: Mutex<HashMap<String, StreamEntry>>,
    /// Serializes opening each camera.
    opening: KeyedLocks,
    grants: Mutex<HashMap<String, CameraGrant>>,
    /// Conversation → cameras its open approval card offers (previews).
    pending: Mutex<HashMap<String, Vec<String>>>,
    /// Session id → session, running or ended (until [`RESULT_TTL`]).
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// The OS prompt is on the host's screen.
    permission_request: std::sync::atomic::AtomicBool,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Closes a camera off the async threads (stopping it blocks briefly).
fn close_stream(stream: Arc<dyn CameraStream>) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(move || drop(stream));
        }
        Err(_) => drop(stream),
    }
}

/// A session's hold on its camera; dropping it lets the camera close.
pub(crate) struct StreamUse {
    pub stream: Arc<dyn CameraStream>,
    device_id: String,
    inner: Weak<Inner>,
}

impl Drop for StreamUse {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Some(entry) = locked(&inner.streams).get_mut(&self.device_id) {
                entry.users = entry.users.saturating_sub(1);
                entry.last_viewed = Instant::now();
            }
        }
    }
}

/// Shrinks a BGRA frame by an integer factor so it is at most `max_width`
/// wide (box filter).
pub(crate) fn shrink(frame: &Frame, max_width: u32) -> Frame {
    let factor = frame.width.div_ceil(max_width.max(1)).max(1);
    if factor == 1 {
        return Frame {
            width: frame.width,
            height: frame.height,
            bgra: frame.bgra.clone(),
        };
    }
    let width = (frame.width / factor).max(1);
    let height = (frame.height / factor).max(1);
    let mut bgra = vec![0u8; (width * height * 4) as usize];
    let area = u32::pow(factor, 2);
    for y in 0..height {
        for x in 0..width {
            let mut sums = [0u32; 4];
            for dy in 0..factor {
                let row = ((y * factor + dy) * frame.width) as usize * 4;
                for dx in 0..factor {
                    let pixel = row + ((x * factor + dx) as usize) * 4;
                    for (sum, value) in sums.iter_mut().zip(&frame.bgra[pixel..pixel + 4]) {
                        *sum += u32::from(*value);
                    }
                }
            }
            let out = ((y * width + x) * 4) as usize;
            for (channel, sum) in sums.iter().enumerate() {
                bgra[out + channel] = (sum / area) as u8;
            }
        }
    }
    Frame {
        width,
        height,
        bgra,
    }
}

/// JPEG of a BGRA frame, shrunk to at most `max_width` wide.
pub(crate) fn encode_jpeg(
    frame: &Frame,
    max_width: u32,
    quality: u8,
) -> Result<Vec<u8>, CameraError> {
    let shrunk;
    let frame = if frame.width > max_width {
        shrunk = shrink(frame, max_width);
        &shrunk
    } else {
        frame
    };
    let (width, height) = match (u16::try_from(frame.width), u16::try_from(frame.height)) {
        (Ok(width), Ok(height)) => (width, height),
        _ => {
            return Err(CameraError::new(
                "FAILED",
                "the frame is too large to encode",
            ))
        }
    };
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, quality)
        .encode(&frame.bgra, width, height, jpeg_encoder::ColorType::Bgra)
        .map_err(|error| CameraError::new("FAILED", format!("cannot encode a frame: {error}")))?;
    Ok(jpeg)
}

impl AgentCamera {
    pub(crate) async fn load(data_dir: &Path) -> Result<Self, AppError> {
        let path = data_dir.join(SETTINGS_FILE);
        let settings = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "unreadable agent camera settings; the camera stays off");
                CameraSettings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CameraSettings::default(),
            Err(error) => return Err(error.into()),
        };
        let camera = Self {
            inner: Arc::new(Inner {
                path,
                settings: RwLock::new(settings),
                host: std::sync::RwLock::new(platform::native()),
                streams: Mutex::new(HashMap::new()),
                opening: KeyedLocks::default(),
                grants: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                permission_request: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        camera.spawn_sweeper();
        Ok(camera)
    }

    /// Closes cameras nobody uses or watches, and forgets old results.
    fn spawn_sweeper(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                AgentCamera { inner }.sweep();
            }
        });
    }

    fn sweep(&self) {
        let idle: Vec<Arc<dyn CameraStream>> = {
            let mut streams = locked(&self.inner.streams);
            let ids: Vec<String> = streams
                .iter()
                .filter(|(_, entry)| {
                    entry.users == 0 && entry.last_viewed.elapsed() >= PREVIEW_IDLE
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| streams.remove(id))
                .map(|entry| entry.stream)
                .collect()
        };
        for stream in idle {
            close_stream(stream);
        }
        locked(&self.inner.sessions)
            .retain(|_, session| session.ended_for().is_none_or(|ended| ended < RESULT_TTL));
    }

    pub(crate) fn host(&self) -> Arc<dyn CameraHost> {
        self.inner
            .host
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn set_host(&self, host: Arc<dyn CameraHost>) {
        *self
            .inner
            .host
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = host;
    }

    pub(crate) async fn settings(&self) -> CameraSettings {
        self.inner.settings.read().await.clone()
    }

    /// Persists `update`. Returns the conversations whose sessions ended
    /// and the ones whose grant was dropped: turning the camera off ends
    /// every session and grant; taking a camera off the list ends those
    /// that use it.
    pub(crate) async fn update_settings(
        &self,
        update: SettingsUpdate,
    ) -> Result<(CameraSettings, Vec<Arc<Session>>, Vec<String>), AppError> {
        let mut settings = self.inner.settings.write().await;
        let mut next = settings.clone();
        if let Some(enabled) = update.enabled {
            next.enabled = enabled;
        }
        if let Some(ids) = update.allowed {
            let connected: Vec<CameraDevice> = self.host().devices().await.unwrap_or_default();
            let mut allowed = Vec::new();
            for id in ids {
                if allowed.iter().any(|camera: &AllowedCamera| camera.id == id) {
                    continue;
                }
                let name = connected
                    .iter()
                    .find(|device| device.id == id)
                    .map(|device| device.name.clone())
                    .or_else(|| {
                        settings
                            .allowed
                            .iter()
                            .find(|camera| camera.id == id)
                            .map(|camera| camera.name.clone())
                    })
                    .ok_or_else(|| {
                        AppError::InvalidRequest(format!("no camera {id} on this computer"))
                    })?;
                allowed.push(AllowedCamera { id, name });
            }
            next.allowed = allowed;
        }
        if let Some(default) = update.default_device {
            next.default_device = match default {
                Value::Null => None,
                Value::String(id) => Some(id),
                _ => {
                    return Err(AppError::InvalidRequest(
                        "defaultDevice must be a camera id or null".to_owned(),
                    ))
                }
            };
        }
        if next
            .default_device
            .as_deref()
            .is_some_and(|id| !next.allows(id))
        {
            next.default_device = None;
        }
        if let Some(capture) = update.capture {
            capture.validate().map_err(AppError::InvalidRequest)?;
            next.capture = capture;
        }
        if let Some(seconds) = update.max_seconds {
            if !(MAX_SECONDS_RANGE.0..=MAX_SECONDS_RANGE.1).contains(&seconds) {
                return Err(AppError::InvalidRequest(format!(
                    "maxSeconds must be between {} and {}",
                    MAX_SECONDS_RANGE.0, MAX_SECONDS_RANGE.1
                )));
            }
            next.max_seconds = seconds;
        }
        if let Some(frames) = update.max_frames {
            if !(MAX_FRAMES_RANGE.0..=MAX_FRAMES_RANGE.1).contains(&frames) {
                return Err(AppError::InvalidRequest(format!(
                    "maxFrames must be between {} and {}",
                    MAX_FRAMES_RANGE.0, MAX_FRAMES_RANGE.1
                )));
            }
            next.max_frames = frames;
        }
        let bytes = serde_json::to_vec_pretty(&next)?;
        let path = self.inner.path.clone();
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&path, &bytes))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        *settings = next.clone();
        drop(settings);
        let (ended, revoked) = if next.enabled {
            let gone = |device_id: &str| !next.allows(device_id);
            (
                self.stop_where(|session| gone(&session.device.id), SessionEnd::Revoked),
                self.drop_grants_where(|grant| gone(&grant.device.id)),
            )
        } else {
            (
                self.stop_where(|_| true, SessionEnd::Revoked),
                self.drop_grants_where(|_| true),
            )
        };
        Ok((next, ended, revoked))
    }

    /// What settings show: support, the OS permission, every connected or
    /// allowed camera, and the defaults agents' sessions start from.
    pub(crate) async fn status_view(&self) -> Value {
        let settings = self.settings().await;
        let host = self.host();
        let reason = host.unsupported_reason();
        let connected = if reason.is_none() {
            host.devices().await.unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot list the cameras");
                Vec::new()
            })
        } else {
            Vec::new()
        };
        let mut devices: Vec<Value> = connected
            .iter()
            .map(|device| {
                serde_json::json!({
                    "id": device.id,
                    "name": device.name,
                    "connected": true,
                    "allowed": settings.allows(&device.id),
                    "inUse": self.running_on(&device.id).is_some(),
                })
            })
            .collect();
        for allowed in &settings.allowed {
            if !connected.iter().any(|device| device.id == allowed.id) {
                devices.push(serde_json::json!({
                    "id": allowed.id,
                    "name": allowed.name,
                    "connected": false,
                    "allowed": true,
                    "inUse": false,
                }));
            }
        }
        serde_json::json!({
            "supported": reason.is_none(),
            "reason": reason,
            "host": crate::computer::host_name(),
            "platform": std::env::consts::OS,
            "permission": host.permission(),
            "permissionRequest": self.permission_requested(),
            "devices": devices,
            "defaultDevice": settings.preferred().map(|camera| camera.id.clone()),
            "capture": settings.capture,
            "maxSeconds": settings.max_seconds,
            "maxFrames": settings.max_frames,
            "limits": {
                "fps": { "min": engine::MIN_FIXED_FPS, "max": engine::MAX_FIXED_FPS },
                "sensitivity": { "min": 1, "max": 100 },
                "maxSeconds": { "min": MAX_SECONDS_RANGE.0, "max": MAX_SECONDS_RANGE.1 },
                "maxFrames": { "min": MAX_FRAMES_RANGE.0, "max": MAX_FRAMES_RANGE.1 },
            },
        })
    }

    /// Asks the OS for camera access on the host (its prompt shows there),
    /// in the background; [`Self::permission_requested`] says while.
    pub(crate) fn request_permission(&self) {
        use std::sync::atomic::Ordering;
        if self.host().permission() != CameraPermission::NotDetermined
            || self.inner.permission_request.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let camera = self.clone();
        tokio::spawn(async move {
            let permission = camera.host().request_permission().await;
            tracing::info!(?permission, "camera permission answered on the host");
            camera
                .inner
                .permission_request
                .store(false, Ordering::SeqCst);
        });
    }

    pub(crate) fn permission_requested(&self) -> bool {
        self.inner
            .permission_request
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn grant(&self, conversation_id: &str) -> Option<CameraGrant> {
        locked(&self.inner.grants).get(conversation_id).cloned()
    }

    pub(crate) fn set_grant(&self, conversation_id: &str, grant: CameraGrant) {
        locked(&self.inner.grants).insert(conversation_id.to_owned(), grant);
    }

    fn drop_grants_where(&self, matches: impl Fn(&CameraGrant) -> bool) -> Vec<String> {
        let mut grants = locked(&self.inner.grants);
        let dropped: Vec<String> = grants
            .iter()
            .filter(|(_, grant)| matches(grant))
            .map(|(conversation, _)| conversation.clone())
            .collect();
        for conversation in &dropped {
            grants.remove(conversation);
        }
        dropped
    }

    /// The approval card of `conversation_id` offers `devices`: they may be
    /// previewed until [`Self::clear_pending`].
    pub(crate) fn set_pending(&self, conversation_id: &str, devices: Vec<String>) {
        locked(&self.inner.pending).insert(conversation_id.to_owned(), devices);
    }

    pub(crate) fn clear_pending(&self, conversation_id: &str) {
        locked(&self.inner.pending).remove(conversation_id);
    }

    /// The camera `device_id`, opened if it is not yet.
    async fn stream(&self, device_id: &str) -> Result<Arc<dyn CameraStream>, CameraError> {
        if let Some(entry) = locked(&self.inner.streams).get_mut(device_id) {
            entry.last_viewed = Instant::now();
            return Ok(entry.stream.clone());
        }
        let lock = self.inner.opening.get(device_id);
        let _opening = lock.lock().await;
        if let Some(entry) = locked(&self.inner.streams).get_mut(device_id) {
            entry.last_viewed = Instant::now();
            return Ok(entry.stream.clone());
        }
        let stream = self.host().open(device_id, FRAME_MAX_WIDTH).await?;
        locked(&self.inner.streams).insert(
            device_id.to_owned(),
            StreamEntry {
                stream: stream.clone(),
                users: 0,
                last_viewed: Instant::now(),
                preview: None,
            },
        );
        Ok(stream)
    }

    /// Holds the camera for a session.
    pub(crate) async fn use_stream(&self, device_id: &str) -> Result<StreamUse, CameraError> {
        let stream = self.stream(device_id).await?;
        if let Some(entry) = locked(&self.inner.streams).get_mut(device_id) {
            entry.users += 1;
        }
        Ok(StreamUse {
            stream,
            device_id: device_id.to_owned(),
            inner: Arc::downgrade(&self.inner),
        })
    }

    /// Whether the camera is open (its light is on).
    #[cfg(test)]
    pub(crate) fn is_open(&self, device_id: &str) -> bool {
        locked(&self.inner.streams).contains_key(device_id)
    }

    /// A small JPEG of what `device_id` sees now, opening it for
    /// [`PREVIEW_IDLE`] if needed. Shared by every viewer.
    pub(crate) async fn preview(&self, device_id: &str) -> Result<Arc<Vec<u8>>, CameraError> {
        if let Some((taken, jpeg)) =
            locked(&self.inner.streams)
                .get_mut(device_id)
                .and_then(|entry| {
                    entry.last_viewed = Instant::now();
                    entry.preview.clone()
                })
        {
            if taken.elapsed() < PREVIEW_INTERVAL {
                return Ok(jpeg);
            }
        }
        let stream = self.stream(device_id).await?;
        // A camera that just started has no frame for a moment.
        let deadline = Instant::now() + Duration::from_secs(3);
        let frame = loop {
            if let Some((_, frame)) = stream.latest(0)? {
                break frame;
            }
            if Instant::now() >= deadline {
                return Err(CameraError::new(
                    "NO_FRAME",
                    "the camera delivers no picture",
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let jpeg = tokio::task::spawn_blocking(move || {
            encode_jpeg(&frame, PREVIEW_MAX_WIDTH, PREVIEW_QUALITY)
        })
        .await
        .map_err(|error| CameraError::new("FAILED", error.to_string()))??;
        let jpeg = Arc::new(jpeg);
        if let Some(entry) = locked(&self.inner.streams).get_mut(device_id) {
            entry.preview = Some((Instant::now(), jpeg.clone()));
        }
        Ok(jpeg)
    }

    /// What a conversation's viewers may see: its running session's camera,
    /// else `device_id` when its approval card offers it.
    pub(crate) fn viewable(
        &self,
        conversation_id: &str,
        device_id: Option<&str>,
    ) -> Option<String> {
        if let Some(session) = self.running_for(conversation_id) {
            return Some(session.device.id.clone());
        }
        let pending = locked(&self.inner.pending);
        let offered = pending.get(conversation_id)?;
        match device_id {
            Some(id) => offered
                .iter()
                .any(|offer| offer == id)
                .then(|| id.to_owned()),
            None => offered.first().cloned(),
        }
    }

    /// The conversation's running session.
    pub(crate) fn running_for(&self, conversation_id: &str) -> Option<Arc<Session>> {
        locked(&self.inner.sessions)
            .values()
            .find(|session| session.conversation_id == conversation_id && session.is_running())
            .cloned()
    }

    /// The running session on `device_id`, of any conversation.
    pub(crate) fn running_on(&self, device_id: &str) -> Option<Arc<Session>> {
        locked(&self.inner.sessions)
            .values()
            .find(|session| session.device.id == device_id && session.is_running())
            .cloned()
    }

    /// The conversation's session `id`, or its newest one.
    pub(crate) fn session_for(
        &self,
        conversation_id: &str,
        id: Option<&str>,
    ) -> Option<Arc<Session>> {
        let sessions = locked(&self.inner.sessions);
        match id {
            Some(id) => sessions
                .get(id)
                .filter(|session| session.conversation_id == conversation_id)
                .cloned(),
            None => sessions
                .values()
                .filter(|session| session.conversation_id == conversation_id)
                .max_by_key(|session| session.started)
                .cloned(),
        }
    }

    /// Starts a session; it records until stopped or a limit is hit, then
    /// journals its end.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        conversations: ConversationSupervisor,
        conversation_id: &str,
        device: CameraDevice,
        capture: CaptureSettings,
        max_seconds: u32,
        max_frames: u32,
        dir: PathBuf,
    ) -> Result<Arc<Session>, CameraError> {
        let stream = self.use_stream(&device.id).await?;
        let session = Arc::new(Session::new(
            conversation_id,
            device,
            capture,
            max_seconds,
            max_frames,
            dir,
        ));
        locked(&self.inner.sessions).insert(session.id.clone(), session.clone());
        tokio::spawn(session::record(session.clone(), stream, conversations));
        Ok(session)
    }

    /// Ends the sessions `matches` picks; returns them.
    fn stop_where(
        &self,
        matches: impl Fn(&Session) -> bool,
        reason: SessionEnd,
    ) -> Vec<Arc<Session>> {
        let running: Vec<Arc<Session>> = locked(&self.inner.sessions)
            .values()
            .filter(|session| session.is_running() && matches(session))
            .cloned()
            .collect();
        for session in &running {
            session.stop(reason);
        }
        running
    }

    /// Ends every session and drops every grant (desktop tools turned off).
    pub(crate) fn revoke_all(&self) -> Vec<Arc<Session>> {
        self.drop_grants_where(|_| true);
        locked(&self.inner.pending).clear();
        self.stop_where(|_| true, SessionEnd::Revoked)
    }

    /// Ends the conversation's running session, if any.
    pub(crate) fn stop_conversation(
        &self,
        conversation_id: &str,
        reason: SessionEnd,
    ) -> Option<Arc<Session>> {
        self.stop_where(|session| session.conversation_id == conversation_id, reason)
            .into_iter()
            .next()
    }

    /// Drops the conversation's grant and ends its session for `reason`.
    /// Returns the grant and the session.
    pub(crate) fn revoke(
        &self,
        conversation_id: &str,
        reason: SessionEnd,
    ) -> (Option<CameraGrant>, Option<Arc<Session>>) {
        let grant = locked(&self.inner.grants).remove(conversation_id);
        self.clear_pending(conversation_id);
        (grant, self.stop_conversation(conversation_id, reason))
    }

    /// Conversation deleted: also forget its sessions and delete their
    /// frames.
    pub(crate) async fn forget(&self, conversation_id: &str) {
        self.revoke(conversation_id, SessionEnd::Revoked);
        let sessions: Vec<Arc<Session>> = {
            let mut sessions = locked(&self.inner.sessions);
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, session)| session.conversation_id == conversation_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for session in sessions {
            session.ended().await;
            if let Err(error) = tokio::fs::remove_dir_all(&session.dir).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%error, dir = %session.dir.display(), "cannot delete a camera session's frames");
                }
            }
        }
    }
}

/// No cameras on this host's platform yet.
#[allow(dead_code)]
pub(crate) struct Unsupported;

#[async_trait]
impl CameraHost for Unsupported {
    fn unsupported_reason(&self) -> Option<String> {
        Some(format!(
            "The agent camera is not available on {} yet (macOS only).",
            std::env::consts::OS
        ))
    }

    fn permission(&self) -> CameraPermission {
        CameraPermission::Denied
    }

    async fn request_permission(&self) -> CameraPermission {
        CameraPermission::Denied
    }

    async fn devices(&self) -> Result<Vec<CameraDevice>, CameraError> {
        Ok(Vec::new())
    }

    async fn open(
        &self,
        _device_id: &str,
        _max_width: u32,
    ) -> Result<Arc<dyn CameraStream>, CameraError> {
        Err(CameraError::new(
            "UNSUPPORTED",
            self.unsupported_reason().unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests;
