//! A scripted camera host, shared with the `todex_desktop` tool tests, and
//! the camera's own tests.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;

use super::*;
use crate::camera::engine::{CaptureMode, CaptureSettings};

/// Cameras that draw a picture: still, or (while `moving`) a bright square
/// sliding across it. Every read is a new frame.
pub(crate) struct FakeCameras {
    pub devices: Mutex<Vec<CameraDevice>>,
    pub permission: Mutex<CameraPermission>,
    pub moving: Arc<AtomicBool>,
    /// Cameras open right now.
    pub open: Arc<AtomicUsize>,
    pub opened: AtomicUsize,
}

impl FakeCameras {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            devices: Mutex::new(vec![
                CameraDevice {
                    id: "cam-front".to_owned(),
                    name: "Front camera".to_owned(),
                },
                CameraDevice {
                    id: "cam-desk".to_owned(),
                    name: "Desk camera".to_owned(),
                },
            ]),
            permission: Mutex::new(CameraPermission::Granted),
            moving: Arc::new(AtomicBool::new(false)),
            open: Arc::new(AtomicUsize::new(0)),
            opened: AtomicUsize::new(0),
        })
    }
}

struct FakeStream {
    seq: AtomicU64,
    moving: Arc<AtomicBool>,
    open: Arc<AtomicUsize>,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CameraStream for FakeStream {
    fn latest(&self, _since: u64) -> Result<Option<(u64, Frame)>, CameraError> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let moving = self.moving.load(Ordering::SeqCst);
        let (width, height) = (320u32, 180u32);
        let left = (seq as u32 * 17) % (width - 60);
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let value = if moving && (left..left + 60).contains(&x) && (50..130).contains(&y) {
                    240
                } else {
                    30 + (x / 40) as u8
                };
                bgra.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Ok(Some((
            seq,
            Frame {
                width,
                height,
                bgra,
            },
        )))
    }
}

#[async_trait]
impl CameraHost for FakeCameras {
    fn unsupported_reason(&self) -> Option<String> {
        None
    }

    fn permission(&self) -> CameraPermission {
        *self.permission.lock().unwrap()
    }

    async fn request_permission(&self) -> CameraPermission {
        let mut permission = self.permission.lock().unwrap();
        if *permission == CameraPermission::NotDetermined {
            *permission = CameraPermission::Granted;
        }
        *permission
    }

    async fn devices(&self) -> Result<Vec<CameraDevice>, CameraError> {
        Ok(self.devices.lock().unwrap().clone())
    }

    async fn open(
        &self,
        device_id: &str,
        _max_width: u32,
    ) -> Result<Arc<dyn CameraStream>, CameraError> {
        if !self
            .devices
            .lock()
            .unwrap()
            .iter()
            .any(|device| device.id == device_id)
        {
            return Err(CameraError::new(
                "UNAVAILABLE",
                "the camera is not connected",
            ));
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeStream {
            seq: AtomicU64::new(0),
            moving: self.moving.clone(),
            open: self.open.clone(),
        }))
    }
}

impl AgentCamera {
    /// Lets every camera's preview look older than [`PREVIEW_IDLE`].
    pub(crate) fn age_streams(&self) {
        for entry in locked(&self.inner.streams).values_mut() {
            entry.last_viewed = Instant::now() - PREVIEW_IDLE - Duration::from_millis(1);
        }
    }

    pub(crate) fn sweep_for_tests(&self) {
        self.sweep();
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("todex-camera-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn camera(dir: &Path) -> (AgentCamera, Arc<FakeCameras>) {
    let camera = AgentCamera::load(dir).await.unwrap();
    let host = FakeCameras::new();
    camera.set_host(host.clone());
    (camera, host)
}

fn allow(ids: &[&str]) -> SettingsUpdate {
    SettingsUpdate {
        enabled: Some(true),
        allowed: Some(ids.iter().map(|id| (*id).to_owned()).collect()),
        ..SettingsUpdate::default()
    }
}

#[tokio::test]
async fn settings_persist_resolve_names_and_validate() {
    let dir = temp_dir("settings");
    let (camera, host) = camera(&dir).await;
    assert!(!camera.settings().await.enabled);
    let (settings, _, _) = camera
        .update_settings(SettingsUpdate {
            default_device: Some(Value::from("cam-desk")),
            capture: Some(CaptureSettings {
                mode: CaptureMode::Fixed,
                fps: 2.0,
                sensitivity: 50,
            }),
            ..allow(&["cam-desk", "cam-front", "cam-desk"])
        })
        .await
        .unwrap();
    assert_eq!(
        settings.allowed,
        vec![
            AllowedCamera {
                id: "cam-desk".to_owned(),
                name: "Desk camera".to_owned()
            },
            AllowedCamera {
                id: "cam-front".to_owned(),
                name: "Front camera".to_owned()
            },
        ]
    );
    assert_eq!(settings.preferred().unwrap().id, "cam-desk");

    // Reloaded from disk; an unplugged allowed camera keeps its name.
    host.devices
        .lock()
        .unwrap()
        .retain(|device| device.id != "cam-desk");
    let (reloaded, _) = super::tests::camera(&dir).await;
    reloaded.set_host(host.clone());
    assert_eq!(reloaded.settings().await, settings);
    let view = reloaded.status_view().await;
    let desk = view["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == "cam-desk")
        .unwrap()
        .clone();
    assert_eq!(desk["connected"], false);
    assert_eq!(desk["name"], "Desk camera");
    assert_eq!(view["permission"], "granted");

    // Unknown cameras, bad limits and capture settings are refused.
    assert!(reloaded.update_settings(allow(&["nope"])).await.is_err());
    for update in [
        SettingsUpdate {
            max_seconds: Some(5),
            ..SettingsUpdate::default()
        },
        SettingsUpdate {
            max_frames: Some(5000),
            ..SettingsUpdate::default()
        },
        SettingsUpdate {
            capture: Some(CaptureSettings {
                sensitivity: 0,
                ..CaptureSettings::default()
            }),
            ..SettingsUpdate::default()
        },
        SettingsUpdate {
            default_device: Some(Value::from(3)),
            ..SettingsUpdate::default()
        },
    ] {
        assert!(reloaded.update_settings(update).await.is_err());
    }
    // The default must be allowed.
    let (settings, _, _) = reloaded
        .update_settings(allow(&["cam-front"]))
        .await
        .unwrap();
    assert_eq!(settings.default_device, None);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn previews_share_one_open_camera_that_closes_when_unwatched() {
    let dir = temp_dir("preview");
    let (camera, host) = camera(&dir).await;
    let first = camera.preview("cam-front").await.unwrap();
    assert_eq!(&first[..2], &[0xFF, 0xD8], "a JPEG");
    camera.preview("cam-front").await.unwrap();
    assert_eq!(host.opened.load(Ordering::SeqCst), 1);
    assert!(camera.is_open("cam-front"));
    // Still watched: stays open.
    camera.sweep_for_tests();
    assert!(camera.is_open("cam-front"));
    camera.age_streams();
    camera.sweep_for_tests();
    assert!(!camera.is_open("cam-front"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(host.open.load(Ordering::SeqCst), 0, "the camera was closed");
    assert!(camera.preview("cam-gone").await.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

/// A session on `cam-front` writing into `dir/frames`.
async fn start(
    camera: &AgentCamera,
    dir: &Path,
    capture: CaptureSettings,
    max_frames: u32,
) -> Arc<Session> {
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    let state = crate::app_state::AppState::new_for_tests(crate::config::Config {
        data_dir: dir.join("data"),
        ..crate::config::Config::default()
    })
    .await
    .unwrap();
    camera
        .start(
            state.conversations.clone(),
            "conv",
            CameraDevice {
                id: "cam-front".to_owned(),
                name: "Front camera".to_owned(),
            },
            capture,
            60,
            max_frames,
            frames,
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_sessions_keep_frames_while_the_picture_moves() {
    let dir = temp_dir("dynamic");
    let (camera, host) = camera(&dir).await;
    let session = start(&camera, &dir, CaptureSettings::default(), 600).await;
    assert!(camera.running_for("conv").is_some());
    assert!(camera.running_on("cam-front").is_some());
    // Still through the baseline, then motion.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let still_frames = session.progress().frames;
    assert_eq!(still_frames, 1, "only the first frame while still");
    host.moving.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    session.stop(SessionEnd::Agent);
    let summary = session.ended().await;
    assert_eq!(summary.reason, SessionEnd::Agent);
    assert!(summary.error.is_none(), "{:?}", summary.error);
    assert!(summary.frames.len() >= 4, "{:?}", summary.frames);
    assert!(!summary.activity.is_empty());
    assert_eq!(summary.frames[0].reason, engine::KeepReason::First);
    assert!(summary
        .frames
        .iter()
        .any(|frame| frame.reason == engine::KeepReason::Change));
    // Names carry the time; files and the manifest are on disk.
    for (index, frame) in summary.frames.iter().enumerate() {
        assert_eq!(frame.file, engine::frame_name(index + 1, frame.offset_ms));
        let bytes = std::fs::read(session.dir.join(&frame.file)).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
    }
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&summary.manifest).unwrap()).unwrap();
    assert_eq!(manifest["frameCount"], summary.frames.len());
    assert_eq!(manifest["endReason"], "agent");
    assert_eq!(manifest["capture"]["mode"], "dynamic");
    assert_eq!(manifest["camera"]["name"], "Front camera");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&summary.manifest)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "owner only");
    }
    assert!(camera.running_for("conv").is_none());
    // The camera closes once nobody uses it.
    camera.age_streams();
    camera.sweep_for_tests();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(host.open.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_sessions_are_evenly_spaced_and_stop_at_the_frame_limit() {
    let dir = temp_dir("fixed");
    let (camera, _host) = camera(&dir).await;
    let capture = CaptureSettings {
        mode: CaptureMode::Fixed,
        fps: 5.0,
        ..CaptureSettings::default()
    };
    let session = start(&camera, &dir, capture, 10).await;
    let summary = tokio::time::timeout(Duration::from_secs(10), session.ended())
        .await
        .unwrap();
    assert_eq!(summary.reason, SessionEnd::FrameLimit);
    assert_eq!(summary.frames.len(), 10);
    // Few frames: sent as they are, no sheet.
    assert_eq!(summary.overview.len(), 10);
    assert!(summary.overview.iter().all(|image| image.file.is_none()));
    assert!(summary.frames[1..]
        .iter()
        .all(|frame| frame.reason == engine::KeepReason::Interval));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_with_many_frames_get_overview_sheets() {
    let dir = temp_dir("sheets");
    let (camera, _host) = camera(&dir).await;
    let capture = CaptureSettings {
        mode: CaptureMode::Fixed,
        fps: 5.0,
        ..CaptureSettings::default()
    };
    let session = start(&camera, &dir, capture, 20).await;
    let summary = tokio::time::timeout(Duration::from_secs(15), session.ended())
        .await
        .unwrap();
    assert_eq!(summary.frames.len(), 20);
    // Every frame on two sheets (16 cells each), saved beside them and
    // listed in the manifest.
    let files: Vec<Option<&str>> = summary
        .overview
        .iter()
        .map(|image| image.file.as_deref())
        .collect();
    assert_eq!(files, vec![Some("overview_1.jpg"), Some("overview_2.jpg")]);
    let shown: Vec<usize> = summary
        .overview
        .iter()
        .flat_map(|image| image.frames.clone())
        .collect();
    assert_eq!(shown, (1..=20).collect::<Vec<_>>());
    assert_eq!(summary.overview[0].frames.len(), 16);
    let bytes = std::fs::read(session.dir.join("overview_2.jpg")).unwrap();
    assert_eq!(bytes, summary.overview[1].jpeg);
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&summary.manifest).unwrap()).unwrap();
    assert_eq!(manifest["overview"][1]["file"], "overview_2.jpg");
    assert_eq!(
        manifest["overview"][1]["frames"].as_array().unwrap().len(),
        4
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_the_camera_off_ends_sessions_and_grants() {
    let dir = temp_dir("off");
    let (camera, _host) = camera(&dir).await;
    camera.update_settings(allow(&["cam-front"])).await.unwrap();
    camera.set_grant(
        "conv",
        CameraGrant {
            device: CameraDevice {
                id: "cam-front".to_owned(),
                name: "Front camera".to_owned(),
            },
            capture: CaptureSettings::default(),
        },
    );
    let session = start(&camera, &dir, CaptureSettings::default(), 600).await;
    // Taking another camera off the list leaves this one alone.
    let (_, ended, revoked) = camera.update_settings(allow(&["cam-front"])).await.unwrap();
    assert!(ended.is_empty() && revoked.is_empty());
    let (_, ended, revoked) = camera
        .update_settings(SettingsUpdate {
            enabled: Some(false),
            ..SettingsUpdate::default()
        })
        .await
        .unwrap();
    assert_eq!(ended.len(), 1);
    assert_eq!(revoked, vec!["conv".to_owned()]);
    assert_eq!(session.ended().await.reason, SessionEnd::Revoked);
    assert!(camera.grant("conv").is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forgetting_a_conversation_deletes_its_frames() {
    let dir = temp_dir("forget");
    let (camera, _host) = camera(&dir).await;
    let session = start(&camera, &dir, CaptureSettings::default(), 600).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    camera.forget("conv").await;
    assert!(!session.dir.exists());
    assert!(camera.session_for("conv", None).is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn frames_shrink_to_the_width_limit() {
    let frame = Frame {
        width: 1920,
        height: 1080,
        bgra: vec![100; 1920 * 1080 * 4],
    };
    let small = shrink(&frame, 640);
    assert_eq!((small.width, small.height), (640, 360));
    assert!(small.bgra.iter().all(|value| *value == 100));
    let jpeg = encode_jpeg(&frame, 1280, 70).unwrap();
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
}
